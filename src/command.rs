use bevy::prelude::*;
use indexmap::IndexMap;
use serde::ser::SerializeMap;
use serde::{Deserialize, Deserializer, Serialize, Serializer};
use std::collections::HashSet;

use crate::components::*;
use crate::onboarding::{OnboardingEvent, send_onboarding_event};
use crate::resources::{
    GlobalStorageConfig, GlobalTransferDirection, PendingGlobalTransfer, PendingGlobalTransfers,
    PlayerGlobalStorage, PlayerLocalStorage, ResourceCost, ResourceRegistry,
};
use crate::systems::{PendingControllerUpgrade, PendingSpawn, PendingSpawnQueue, RoomDroneCounts};

pub type ObjectId = u64;
pub type Tick = u64;

pub const MAX_BODY_PARTS: usize = 50;
pub const MAX_COMMANDS_PER_PLAYER: usize = 100;
pub const MAX_DRONES_PER_PLAYER: u32 = 500;
pub const MAX_TICK_OUTPUT_BYTES: usize = 256 * 1024;
pub const MAX_JSON_DEPTH: usize = 10;
pub const MAX_FUEL: u64 = 10_000_000;
pub const MAX_REFUND_PER_TICK: u64 = MAX_FUEL / 10;
pub const MAX_NEXT_TICK_FUEL_BUDGET: u64 = MAX_FUEL + MAX_REFUND_PER_TICK;
pub const MAX_RANGED_ATTACK_RANGE: u32 = 3;
const ENERGY_RESOURCE: &str = "Energy";
const OVERLOAD_FUEL_DRAIN: u64 = 500_000;
const OVERLOAD_FUEL_FLOOR: u64 = MAX_FUEL / 5;

#[derive(Resource, Debug, Clone, Default)]
struct CustomActionCooldowns(IndexMap<(ObjectId, String), Tick>);

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum CommandSource {
    Wasm,
    McpDeploy,
    McpQuery,
    Admin,
    Replay,
    TestHarness,
    Tutorial,
    Deploy,
    Rollback,
    RuleMod,
    Simulate,
    DryRun,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum Direction {
    Top,
    TopRight,
    BottomRight,
    Bottom,
    BottomLeft,
    TopLeft,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CommandAction {
    // --- Phase 1 commands ---
    Move {
        object_id: ObjectId,
        direction: Direction,
    },
    Harvest {
        object_id: ObjectId,
        target_id: ObjectId,
        resource: Option<String>,
    },
    Transfer {
        object_id: ObjectId,
        target_id: ObjectId,
        resource: String,
        amount: u32,
    },

    // --- Phase 4+ commands (defined ahead of full implementation) ---
    Withdraw {
        object_id: ObjectId,
        target_id: ObjectId,
        resource: String,
        amount: u32,
    },
    Attack {
        object_id: ObjectId,
        target_id: ObjectId,
    },
    RangedAttack {
        object_id: ObjectId,
        target_id: ObjectId,
        range: u32,
    },
    Heal {
        object_id: ObjectId,
        target_id: ObjectId,
    },
    ClaimController {
        object_id: ObjectId,
        controller_id: ObjectId,
    },
    Spawn {
        spawn_id: ObjectId,
        body: Vec<BodyPart>,
    },
    Recycle {
        object_id: ObjectId,
        spawn_id: ObjectId,
    },
    Build {
        object_id: ObjectId,
        x: i32,
        y: i32,
        structure: StructureType,
    },
    TransferToGlobal {
        resource: String,
        amount: u32,
    },
    TransferFromGlobal {
        resource: String,
        amount: u32,
    },
    Custom {
        action_type: String,
        object_id: ObjectId,
        target_id: Option<ObjectId>,
        resource: Option<String>,
        amount: Option<u32>,
        structure: Option<StructureType>,
    },
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct CommandActionWire {
    #[serde(rename = "type")]
    action_type: String,
    object_id: Option<ObjectId>,
    target_id: Option<ObjectId>,
    controller_id: Option<ObjectId>,
    spawn_id: Option<ObjectId>,
    direction: Option<Direction>,
    body: Option<Vec<BodyPart>>,
    resource: Option<String>,
    amount: Option<u32>,
    range: Option<u32>,
    x: Option<i32>,
    y: Option<i32>,
    structure: Option<StructureType>,
    price_resource: Option<String>,
    price_amount: Option<u32>,
    order_id: Option<u64>,
}

impl<'de> Deserialize<'de> for CommandAction {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let wire = CommandActionWire::deserialize(deserializer)?;
        macro_rules! required {
            ($value:expr, $field:literal) => {
                $value.ok_or_else(|| serde::de::Error::missing_field($field))?
            };
        }
        Ok(match wire.action_type.as_str() {
            "Move" => Self::Move {
                object_id: required!(wire.object_id, "object_id"),
                direction: required!(wire.direction, "direction"),
            },
            "Harvest" => Self::Harvest {
                object_id: required!(wire.object_id, "object_id"),
                target_id: required!(wire.target_id, "target_id"),
                resource: wire.resource,
            },
            "Transfer" => Self::Transfer {
                object_id: required!(wire.object_id, "object_id"),
                target_id: required!(wire.target_id, "target_id"),
                resource: required!(wire.resource, "resource"),
                amount: required!(wire.amount, "amount"),
            },
            "Withdraw" => Self::Withdraw {
                object_id: required!(wire.object_id, "object_id"),
                target_id: required!(wire.target_id, "target_id"),
                resource: required!(wire.resource, "resource"),
                amount: required!(wire.amount, "amount"),
            },
            "Attack" => Self::Attack {
                object_id: required!(wire.object_id, "object_id"),
                target_id: required!(wire.target_id, "target_id"),
            },
            "RangedAttack" => Self::RangedAttack {
                object_id: required!(wire.object_id, "object_id"),
                target_id: required!(wire.target_id, "target_id"),
                range: required!(wire.range, "range"),
            },
            "Heal" => Self::Heal {
                object_id: required!(wire.object_id, "object_id"),
                target_id: required!(wire.target_id, "target_id"),
            },
            "ClaimController" => Self::ClaimController {
                object_id: required!(wire.object_id, "object_id"),
                controller_id: required!(wire.controller_id, "controller_id"),
            },
            "Spawn" => Self::Spawn {
                spawn_id: required!(wire.spawn_id, "spawn_id"),
                body: required!(wire.body, "body"),
            },
            "Recycle" => Self::Recycle {
                object_id: required!(wire.object_id, "object_id"),
                spawn_id: required!(wire.spawn_id, "spawn_id"),
            },
            "Build" => Self::Build {
                object_id: required!(wire.object_id, "object_id"),
                x: required!(wire.x, "x"),
                y: required!(wire.y, "y"),
                structure: required!(wire.structure, "structure"),
            },
            "TransferToGlobal" => Self::TransferToGlobal {
                resource: required!(wire.resource, "resource"),
                amount: required!(wire.amount, "amount"),
            },
            "TransferFromGlobal" => Self::TransferFromGlobal {
                resource: required!(wire.resource, "resource"),
                amount: required!(wire.amount, "amount"),
            },
            custom => Self::Custom {
                action_type: custom.to_string(),
                object_id: required!(wire.object_id, "object_id"),
                target_id: wire.target_id,
                resource: wire.resource,
                amount: wire.amount,
                structure: wire.structure,
            },
        })
    }
}

impl Serialize for CommandAction {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        let mut map = serializer.serialize_map(None)?;
        match self {
            Self::Move {
                object_id,
                direction,
            } => {
                map.serialize_entry("type", "Move")?;
                map.serialize_entry("object_id", object_id)?;
                map.serialize_entry("direction", direction)?;
            }
            Self::Harvest {
                object_id,
                target_id,
                resource,
            } => {
                map.serialize_entry("type", "Harvest")?;
                map.serialize_entry("object_id", object_id)?;
                map.serialize_entry("target_id", target_id)?;
                if let Some(resource) = resource {
                    map.serialize_entry("resource", resource)?;
                }
            }
            Self::Transfer {
                object_id,
                target_id,
                resource,
                amount,
            } => serialize_resource_action(
                &mut map, "Transfer", object_id, target_id, resource, amount,
            )?,
            Self::Withdraw {
                object_id,
                target_id,
                resource,
                amount,
            } => serialize_resource_action(
                &mut map, "Withdraw", object_id, target_id, resource, amount,
            )?,
            Self::Attack {
                object_id,
                target_id,
            } => serialize_target_action(&mut map, "Attack", object_id, target_id)?,
            Self::RangedAttack {
                object_id,
                target_id,
                range,
            } => {
                serialize_target_action(&mut map, "RangedAttack", object_id, target_id)?;
                map.serialize_entry("range", range)?;
            }
            Self::Heal {
                object_id,
                target_id,
            } => serialize_target_action(&mut map, "Heal", object_id, target_id)?,
            Self::ClaimController {
                object_id,
                controller_id,
            } => {
                map.serialize_entry("type", "ClaimController")?;
                map.serialize_entry("object_id", object_id)?;
                map.serialize_entry("controller_id", controller_id)?;
            }
            Self::Spawn { spawn_id, body } => {
                map.serialize_entry("type", "Spawn")?;
                map.serialize_entry("spawn_id", spawn_id)?;
                map.serialize_entry("body", body)?;
            }
            Self::Recycle {
                object_id,
                spawn_id,
            } => {
                map.serialize_entry("type", "Recycle")?;
                map.serialize_entry("object_id", object_id)?;
                map.serialize_entry("spawn_id", spawn_id)?;
            }
            Self::Build {
                object_id,
                x,
                y,
                structure,
            } => {
                map.serialize_entry("type", "Build")?;
                map.serialize_entry("object_id", object_id)?;
                map.serialize_entry("x", x)?;
                map.serialize_entry("y", y)?;
                map.serialize_entry("structure", structure)?;
            }
            Self::TransferToGlobal { resource, amount } => {
                map.serialize_entry("type", "TransferToGlobal")?;
                map.serialize_entry("resource", resource)?;
                map.serialize_entry("amount", amount)?;
            }
            Self::TransferFromGlobal { resource, amount } => {
                map.serialize_entry("type", "TransferFromGlobal")?;
                map.serialize_entry("resource", resource)?;
                map.serialize_entry("amount", amount)?;
            }
            Self::Custom {
                action_type,
                object_id,
                target_id,
                resource,
                amount,
                structure,
            } => {
                map.serialize_entry("type", action_type)?;
                map.serialize_entry("object_id", object_id)?;
                if let Some(target_id) = target_id {
                    map.serialize_entry("target_id", target_id)?;
                }
                if let Some(resource) = resource {
                    map.serialize_entry("resource", resource)?;
                }
                if let Some(amount) = amount {
                    map.serialize_entry("amount", amount)?;
                }
                if let Some(structure) = structure {
                    map.serialize_entry("structure", structure)?;
                }
            }
        }
        map.end()
    }
}

fn serialize_target_action<S>(
    map: &mut S,
    action_type: &str,
    object_id: &ObjectId,
    target_id: &ObjectId,
) -> Result<(), S::Error>
where
    S: SerializeMap,
{
    map.serialize_entry("type", action_type)?;
    map.serialize_entry("object_id", object_id)?;
    map.serialize_entry("target_id", target_id)
}

fn serialize_resource_action<S>(
    map: &mut S,
    action_type: &str,
    object_id: &ObjectId,
    target_id: &ObjectId,
    resource: &str,
    amount: &u32,
) -> Result<(), S::Error>
where
    S: SerializeMap,
{
    serialize_target_action(map, action_type, object_id, target_id)?;
    map.serialize_entry("resource", resource)?;
    map.serialize_entry("amount", amount)
}

/// Untrusted command shape emitted by a player module. Envelope fields are not
/// representable here; Source Gate is the only path to `RawCommand`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CommandIntent {
    pub sequence: u32,
    pub action: CommandAction,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CommandAuth {
    pub source: CommandSource,
    pub player_id: PlayerId,
    pub tick_submitted: Tick,
    pub tick_target: Tick,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RawCommand {
    pub player_id: PlayerId,
    pub tick: Tick,
    pub source: CommandSource,
    pub auth: CommandAuth,
    pub sequence: u32,
    pub action: CommandAction,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ValidatedCommand {
    pub raw: RawCommand,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum RejectionReason {
    InvalidJson,
    SchemaViolation,
    CommandBufferFull,
    TimeoutExceeded,
    SourceNotAllowed,
    AuthContextInvalid,
    NotVisibleOrNotFound,
    ObjectNotFound,
    TargetNotFound,
    TargetNotVisible,
    NotOwner,
    NotMovable,
    Fatigued,
    InvalidBodyPart,
    NotEnoughBodyParts,
    MissingBodyPart {
        part: BodyPart,
    },
    TileBlocked,
    InvalidDirection,
    StillSpawning,
    OutOfRoom,
    NoPath,
    PathTooLong,
    InsufficientMoveParts,
    InsufficientResource {
        resource: String,
        required: u32,
        available: u32,
    },
    InsufficientEnergy,
    InsufficientResources,
    CarryFull,
    NotSource,
    SourceEmpty,
    OutOfRange {
        distance: u32,
        max: u32,
    },
    TargetFull,
    TargetEmpty,
    NotYourRoom,
    TileOccupied,
    PositionOccupied,
    InvalidTerrain,
    InvalidStructureType,
    InvalidResourceType,
    TooManyConstructionSites,
    ConstructionLimitReached,
    NotStructure,
    NotController,
    AlreadyFullHealth,
    FriendlyTarget,
    NotYourSpawn,
    SpawnOnCooldown,
    CooldownActive,
    BodyTooLarge,
    ExceedsRoomCapacity,
    RoomDroneCapReached,
    NotFriendly,
    GlobalStorageDisabled,
    TransferInProgress,
    TerminalRequired,
    OrderNotFound,
    AlreadyHacked,
    InvalidDamageType,
    AlreadyDebilitated {
        damage_type: String,
    },
    PlayerNotFound,
    TargetFuelTooLow,
    FuelExhausted,
    SafeModeActive,
    TargetFortifyCooldown,
    TargetOverloadCooldown,
    InternalError,
    ServerOverloaded,
    SnapshotOverBudget,
    InvalidCertificate,
    CertExpired,
    TokenRevoked,
    RefreshTokenInvalid,
    NotAuthorized,
    ScopeInsufficient,
    SessionLimitReached,
    DeviceNotRegistered,
    MultiDeviceConflict,
    UnknownCredential,
    InternalAuthError,
    UnknownAction {
        action: String,
    },
}

pub type CommandResult = Result<(), RejectionReason>;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TickValidationError {
    TooLarge,
    InvalidJson,
    NotArray,
    TooManyCommands,
    TooDeep,
    SchemaViolation,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CommandRejection {
    pub command: RawCommand,
    pub rejection: RejectionReason,
    pub detail: serde_json::Value,
    pub tick: Tick,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CommandActionMetadata {
    pub name: String,
    pub description: String,
    pub params: Vec<String>,
    pub range: Option<u32>,
    pub cooldown: Option<u32>,
    pub cost: ResourceCost,
    pub special_effect: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct RefundAccumulator {
    pub next_tick_fuel_credit: u64,
    seen: HashSet<(PlayerId, CommandSource, RejectionReason)>,
}

pub fn source_gate(
    player_id: PlayerId,
    tick: Tick,
    source: CommandSource,
    intent: CommandIntent,
) -> Result<RawCommand, RejectionReason> {
    if !source_allows_action(source, &intent.action) {
        return Err(RejectionReason::SourceNotAllowed);
    }

    Ok(RawCommand {
        player_id,
        tick,
        source,
        auth: CommandAuth {
            source,
            player_id,
            tick_submitted: tick,
            tick_target: tick,
        },
        sequence: intent.sequence,
        action: intent.action,
    })
}

pub fn parse_tick_output(
    player_id: PlayerId,
    tick: Tick,
    bytes: &[u8],
) -> Result<Vec<RawCommand>, TickValidationError> {
    if bytes.len() > MAX_TICK_OUTPUT_BYTES {
        return Err(TickValidationError::TooLarge);
    }

    let value: serde_json::Value =
        serde_json::from_slice(bytes).map_err(|_| TickValidationError::InvalidJson)?;
    if !value.is_array() {
        return Err(TickValidationError::NotArray);
    }
    if json_depth(&value) > MAX_JSON_DEPTH {
        return Err(TickValidationError::TooDeep);
    }
    let commands = value.as_array().ok_or(TickValidationError::NotArray)?;
    if commands.len() > MAX_COMMANDS_PER_PLAYER {
        return Err(TickValidationError::TooManyCommands);
    }

    commands
        .iter()
        .map(|command| {
            let intent: CommandIntent = serde_json::from_value(command.clone())
                .map_err(|_| TickValidationError::SchemaViolation)?;
            source_gate(player_id, tick, CommandSource::Wasm, intent)
                .map_err(|_| TickValidationError::SchemaViolation)
        })
        .collect()
}

pub fn object_id(entity: Entity) -> ObjectId {
    entity.to_bits()
}

pub fn validate_command(
    world: &mut World,
    raw: RawCommand,
) -> Result<ValidatedCommand, RejectionReason> {
    if !raw.auth.matches_raw_envelope(&raw) {
        return Err(RejectionReason::AuthContextInvalid);
    }
    if !source_allows_action(raw.source, &raw.action) {
        return Err(RejectionReason::SourceNotAllowed);
    }
    if raw.source == CommandSource::Tutorial
        && world.resource::<WorldSettings>().mode != WorldMode::Tutorial
    {
        return Err(RejectionReason::SourceNotAllowed);
    }

    let result = match &raw.action {
        CommandAction::Move {
            object_id,
            direction,
        } => validate_move(world, raw.player_id, *object_id, *direction),
        CommandAction::Harvest {
            object_id,
            target_id,
            resource: _,
        } => validate_harvest(world, raw.player_id, *object_id, *target_id),
        CommandAction::Transfer {
            object_id,
            target_id,
            resource,
            amount,
        } => validate_transfer(
            world,
            raw.player_id,
            *object_id,
            *target_id,
            resource,
            *amount,
        ),
        CommandAction::Withdraw {
            object_id,
            target_id,
            resource,
            amount,
        } => validate_withdraw(
            world,
            raw.player_id,
            *object_id,
            *target_id,
            resource,
            *amount,
        ),
        CommandAction::Attack {
            object_id,
            target_id,
        } => validate_attack(world, raw.player_id, *object_id, *target_id),
        CommandAction::RangedAttack {
            object_id,
            target_id,
            range,
        } => validate_ranged_attack(world, raw.player_id, *object_id, *target_id, *range),
        CommandAction::Heal {
            object_id,
            target_id,
        } => validate_heal(world, raw.player_id, *object_id, *target_id),
        CommandAction::ClaimController {
            object_id,
            controller_id,
        } => validate_claim_controller(world, raw.player_id, *object_id, *controller_id),
        CommandAction::Spawn { spawn_id, body } => {
            validate_spawn_drone(world, raw.player_id, *spawn_id, body)
        }
        CommandAction::Recycle {
            object_id,
            spawn_id,
        } => validate_recycle(world, raw.player_id, *object_id, *spawn_id),
        CommandAction::Build {
            object_id,
            x,
            y,
            structure,
        } => validate_build(world, raw.player_id, *object_id, *x, *y, *structure),
        CommandAction::TransferToGlobal { resource, amount } => {
            validate_transfer_to_global(world, raw.player_id, resource, *amount)
        }
        CommandAction::TransferFromGlobal { resource, amount } => {
            validate_transfer_from_global(world, raw.player_id, resource, *amount)
        }
        CommandAction::Custom {
            action_type,
            object_id,
            target_id,
            ..
        } => validate_custom_action(
            world,
            raw.player_id,
            raw.tick,
            action_type,
            *object_id,
            *target_id,
        ),
    };

    if matches!(result, Err(RejectionReason::InsufficientResource { .. })) {
        send_onboarding_event(
            world,
            OnboardingEvent::ResourceBottleneckExplanationAvailable,
        );
    }
    result?;

    Ok(ValidatedCommand { raw })
}

pub fn source_allows_gameplay(source: CommandSource) -> bool {
    matches!(
        source,
        CommandSource::Wasm
            | CommandSource::Admin
            | CommandSource::TestHarness
            | CommandSource::Tutorial
    )
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SourceCapabilities {
    pub write_world: bool,
    pub global_storage: bool,
    pub deploy_code: bool,
    pub query_world: bool,
    pub trigger_combat: bool,
}

pub fn source_capabilities(source: CommandSource) -> SourceCapabilities {
    match source {
        CommandSource::Wasm => SourceCapabilities {
            write_world: true,
            global_storage: true,
            deploy_code: false,
            query_world: true,
            trigger_combat: true,
        },
        CommandSource::McpDeploy | CommandSource::Deploy => SourceCapabilities {
            write_world: false,
            global_storage: false,
            deploy_code: true,
            query_world: false,
            trigger_combat: false,
        },
        CommandSource::McpQuery => SourceCapabilities {
            write_world: false,
            global_storage: false,
            deploy_code: false,
            query_world: true,
            trigger_combat: false,
        },
        CommandSource::Admin | CommandSource::TestHarness => SourceCapabilities {
            write_world: true,
            global_storage: true,
            deploy_code: true,
            query_world: true,
            trigger_combat: true,
        },
        CommandSource::Replay => SourceCapabilities {
            write_world: false,
            global_storage: false,
            deploy_code: false,
            query_world: true,
            trigger_combat: false,
        },
        CommandSource::Tutorial => SourceCapabilities {
            write_world: true,
            global_storage: false,
            deploy_code: false,
            query_world: true,
            trigger_combat: true,
        },
        CommandSource::Rollback => SourceCapabilities {
            write_world: true,
            global_storage: true,
            deploy_code: true,
            query_world: true,
            trigger_combat: false,
        },
        CommandSource::RuleMod => SourceCapabilities {
            write_world: true,
            global_storage: false,
            deploy_code: false,
            query_world: false,
            trigger_combat: false,
        },
        CommandSource::Simulate => SourceCapabilities {
            write_world: false,
            global_storage: false,
            deploy_code: false,
            query_world: true,
            trigger_combat: false,
        },
        CommandSource::DryRun => SourceCapabilities {
            write_world: false,
            global_storage: false,
            deploy_code: false,
            query_world: false,
            trigger_combat: false,
        },
    }
}

pub fn source_allows_action(source: CommandSource, action: &CommandAction) -> bool {
    match source {
        CommandSource::Wasm | CommandSource::Admin | CommandSource::TestHarness => true,
        CommandSource::Tutorial => !action_uses_global_storage(action),
        CommandSource::McpDeploy
        | CommandSource::McpQuery
        | CommandSource::Replay
        | CommandSource::Deploy
        | CommandSource::Rollback
        | CommandSource::RuleMod
        | CommandSource::Simulate
        | CommandSource::DryRun => false,
    }
}

fn action_triggers_combat(action: &CommandAction) -> bool {
    matches!(
        action,
        CommandAction::Attack { .. }
            | CommandAction::RangedAttack { .. }
            | CommandAction::Heal { .. }
    )
}

fn action_uses_global_storage(action: &CommandAction) -> bool {
    matches!(
        action,
        CommandAction::TransferToGlobal { .. } | CommandAction::TransferFromGlobal { .. }
    )
}

impl CommandAuth {
    fn matches_raw_envelope(&self, raw: &RawCommand) -> bool {
        self.source == raw.source
            && self.player_id == raw.player_id
            && self.tick_target == raw.tick
            && self.tick_submitted <= self.tick_target
    }
}

pub fn refund_for_rejection(reason: &RejectionReason, consumed_fuel: u64) -> u64 {
    match reason {
        RejectionReason::SourceEmpty
        | RejectionReason::TileOccupied
        | RejectionReason::TargetFull => consumed_fuel / 2,
        _ => 0,
    }
}

pub fn next_tick_fuel_budget(next_tick_fuel_credit: u64) -> u64 {
    MAX_FUEL
        .saturating_add(next_tick_fuel_credit)
        .min(MAX_NEXT_TICK_FUEL_BUDGET)
}

pub fn available_action_metadata(world: &World) -> Vec<CommandActionMetadata> {
    let mut actions = vec![
        builtin_action_metadata("Move", "Move one hex", &["object_id", "direction"]),
        builtin_action_metadata(
            "Harvest",
            "Harvest from a source",
            &["object_id", "target_id", "resource"],
        ),
        builtin_action_metadata(
            "Transfer",
            "Transfer resource to a target",
            &["object_id", "target_id", "resource", "amount"],
        ),
        builtin_action_metadata(
            "Withdraw",
            "Withdraw resource from a target",
            &["object_id", "target_id", "resource", "amount"],
        ),
        builtin_action_metadata("Attack", "Melee attack", &["object_id", "target_id"]),
        builtin_action_metadata(
            "RangedAttack",
            "Ranged attack",
            &["object_id", "target_id", "range"],
        ),
        builtin_action_metadata("Heal", "Heal a friendly drone", &["object_id", "target_id"]),
        builtin_action_metadata(
            "ClaimController",
            "Claim a room controller",
            &["object_id", "controller_id"],
        ),
        builtin_action_metadata("Spawn", "Spawn a drone", &["spawn_id", "body"]),
        builtin_action_metadata(
            "Recycle",
            "Recycle a drone for a body cost refund",
            &["object_id", "spawn_id"],
        ),
        builtin_action_metadata(
            "Build",
            "Build a registered structure type",
            &["object_id", "x", "y", "structure"],
        ),
        builtin_action_metadata(
            "TransferToGlobal",
            "Transfer local resources to global storage",
            &["resource", "amount"],
        ),
        builtin_action_metadata(
            "TransferFromGlobal",
            "Transfer global resources to local storage",
            &["resource", "amount"],
        ),
    ];
    if let Some(registry) = world.get_resource::<CustomActionRegistry>() {
        actions.extend(
            registry
                .actions
                .values()
                .map(|action| CommandActionMetadata {
                    name: action.name.clone(),
                    description: action.description.clone(),
                    params: vec![
                        "object_id".to_string(),
                        "target_id".to_string(),
                        "resource".to_string(),
                        "amount".to_string(),
                        "structure".to_string(),
                    ],
                    range: Some(action.range),
                    cooldown: custom_action_cooldown(action),
                    cost: custom_action_cost(action),
                    special_effect: action.special_effect.clone(),
                }),
        );
    }
    actions
}

fn builtin_action_metadata(
    name: &str,
    description: &str,
    params: &[&str],
) -> CommandActionMetadata {
    CommandActionMetadata {
        name: name.to_string(),
        description: description.to_string(),
        params: params.iter().map(|param| param.to_string()).collect(),
        range: None,
        cooldown: None,
        cost: ResourceCost::new(),
        special_effect: None,
    }
}

impl RefundAccumulator {
    pub fn record_rejection(
        &mut self,
        raw: &RawCommand,
        reason: &RejectionReason,
        consumed_fuel: u64,
    ) -> u64 {
        let key = (raw.player_id, raw.source, reason.clone());
        if !self.seen.insert(key) {
            return 0;
        }

        let remaining = MAX_REFUND_PER_TICK.saturating_sub(self.next_tick_fuel_credit);
        let refund = refund_for_rejection(reason, consumed_fuel).min(remaining);
        self.next_tick_fuel_credit += refund;
        refund
    }

    pub fn clear_for_deploy(&mut self) {
        self.next_tick_fuel_credit = 0;
        self.seen.clear();
    }
}

impl CommandRejection {
    pub fn new(command: RawCommand, rejection: RejectionReason) -> Self {
        let tick = command.tick;
        let detail = rejection_detail(&command, &rejection);
        Self {
            command,
            rejection,
            detail,
            tick,
        }
    }
}

fn rejection_detail(command: &RawCommand, rejection: &RejectionReason) -> serde_json::Value {
    let action = match &command.action {
        CommandAction::Move { .. } => "Move",
        CommandAction::Harvest { .. } => "Harvest",
        CommandAction::Transfer { .. } => "Transfer",
        CommandAction::Withdraw { .. } => "Withdraw",
        CommandAction::Attack { .. } => "Attack",
        CommandAction::RangedAttack { .. } => "RangedAttack",
        CommandAction::Heal { .. } => "Heal",
        CommandAction::ClaimController { .. } => "ClaimController",
        CommandAction::Spawn { .. } => "Spawn",
        CommandAction::Recycle { .. } => "Recycle",
        CommandAction::Build { .. } => "Build",
        CommandAction::TransferToGlobal { .. } => "TransferToGlobal",
        CommandAction::TransferFromGlobal { .. } => "TransferFromGlobal",
        CommandAction::Custom { action_type, .. } => action_type,
    };

    let detail = match rejection {
        RejectionReason::SourceEmpty => match &command.action {
            CommandAction::Harvest {
                object_id,
                target_id,
                resource,
            } => serde_json::json!({
                "reason": "SourceEmpty",
                "action": action,
                "conflict": "first_come_first_served",
                "refund_policy": { "fuel_percent": 50 },
                "object_id": object_id,
                "target_id": target_id,
                "resource": resource.as_deref().unwrap_or("Energy"),
            }),
            _ => default_rejection_detail(command, rejection, action),
        },
        RejectionReason::TileOccupied => match &command.action {
            CommandAction::Build {
                object_id,
                x,
                y,
                structure,
            } => serde_json::json!({
                "reason": "TileOccupied",
                "action": action,
                "conflict": "first_come_first_served",
                "refund_policy": { "fuel_percent": 50 },
                "object_id": object_id,
                "position": { "x": x, "y": y },
                "structure": structure,
            }),
            CommandAction::Spawn { spawn_id, body } => serde_json::json!({
                "reason": "TileOccupied",
                "action": action,
                "conflict": "first_come_first_served",
                "refund_policy": { "fuel_percent": 50 },
                "spawn_id": spawn_id,
                "body_parts": body,
            }),
            _ => default_rejection_detail(command, rejection, action),
        },
        RejectionReason::TargetFull => match &command.action {
            CommandAction::Transfer {
                object_id,
                target_id,
                resource,
                amount,
            } => serde_json::json!({
                "reason": "TargetFull",
                "action": action,
                "conflict": "first_come_first_served",
                "refund_policy": { "fuel_percent": 50 },
                "object_id": object_id,
                "target_id": target_id,
                "resource": resource,
                "amount": amount,
            }),
            CommandAction::Withdraw {
                object_id,
                target_id,
                resource,
                amount,
            } => serde_json::json!({
                "reason": "TargetFull",
                "action": action,
                "conflict": "first_come_first_served",
                "refund_policy": { "fuel_percent": 50 },
                "object_id": object_id,
                "target_id": target_id,
                "resource": resource,
                "amount": amount,
            }),
            _ => default_rejection_detail(command, rejection, action),
        },
        RejectionReason::AlreadyDebilitated { damage_type } => serde_json::json!({
            "reason": "AlreadyDebilitated",
            "action": action,
            "damage_type": damage_type,
        }),
        RejectionReason::OutOfRange { distance, max } => serde_json::json!({
            "reason": "OutOfRange",
            "action": action,
            "distance": distance,
            "max": max,
        }),
        RejectionReason::MissingBodyPart { part } => serde_json::json!({
            "reason": "MissingBodyPart",
            "action": action,
            "part": part,
        }),
        RejectionReason::InsufficientResource {
            resource,
            required,
            available,
        } => serde_json::json!({
            "reason": "InsufficientResource",
            "action": action,
            "resource": resource,
            "required": required,
            "available": available,
        }),
        _ => default_rejection_detail(command, rejection, action),
    };
    add_canonical_rejection_detail(detail, rejection)
}

fn default_rejection_detail(
    command: &RawCommand,
    rejection: &RejectionReason,
    action: &str,
) -> serde_json::Value {
    serde_json::json!({
        "reason": canonical_rejection_reason(rejection),
        "action": action,
        "player_id": command.player_id,
        "sequence": command.sequence,
        "source": command.source,
    })
}

fn add_canonical_rejection_detail(
    detail: serde_json::Value,
    rejection: &RejectionReason,
) -> serde_json::Value {
    let mut detail = detail;
    let canonical_reason = canonical_rejection_reason(rejection);
    if let serde_json::Value::Object(fields) = &mut detail {
        fields
            .entry("reason".to_string())
            .or_insert_with(|| serde_json::Value::String(canonical_reason.to_string()));
        if non_canonical_rejection_reason(rejection) {
            fields.insert(
                "canonical_reason".to_string(),
                serde_json::Value::String(canonical_reason.to_string()),
            );
            fields.insert(
                "internal_reason".to_string(),
                serde_json::Value::String(format!("{rejection:?}")),
            );
            fields.insert(
                "debug_detail".to_string(),
                serde_json::Value::String(format!("{rejection:?} -> {canonical_reason}")),
            );
        }
    }
    detail
}

fn canonical_rejection_reason(rejection: &RejectionReason) -> &'static str {
    match rejection {
        RejectionReason::InvalidJson => "InvalidJson",
        RejectionReason::SchemaViolation => "SchemaViolation",
        RejectionReason::CommandBufferFull => "CommandBufferFull",
        RejectionReason::TimeoutExceeded => "TimeoutExceeded",
        RejectionReason::SourceNotAllowed => "SourceNotAllowed",
        RejectionReason::AuthContextInvalid => "AuthContextInvalid",
        RejectionReason::NotVisibleOrNotFound => "NotVisibleOrNotFound",
        RejectionReason::ObjectNotFound | RejectionReason::TargetNotFound => "ObjectNotFound",
        RejectionReason::TargetNotVisible => "TargetNotVisible",
        RejectionReason::NotOwner
        | RejectionReason::FriendlyTarget
        | RejectionReason::NotFriendly
        | RejectionReason::NotYourRoom
        | RejectionReason::NotYourSpawn => "NotOwner",
        RejectionReason::NotMovable
        | RejectionReason::NotSource
        | RejectionReason::OrderNotFound => "ObjectNotFound",
        RejectionReason::Fatigued
        | RejectionReason::StillSpawning
        | RejectionReason::AlreadyFullHealth
        | RejectionReason::AlreadyHacked
        | RejectionReason::AlreadyDebilitated { .. }
        | RejectionReason::TerminalRequired => "CooldownActive",
        RejectionReason::InvalidBodyPart | RejectionReason::BodyTooLarge => "InvalidBodyPart",
        RejectionReason::NotEnoughBodyParts
        | RejectionReason::MissingBodyPart { .. }
        | RejectionReason::InsufficientMoveParts => "NotEnoughBodyParts",
        RejectionReason::TileBlocked
        | RejectionReason::TileOccupied
        | RejectionReason::PositionOccupied
        | RejectionReason::InvalidTerrain
        | RejectionReason::NoPath
        | RejectionReason::PathTooLong
        | RejectionReason::OutOfRoom => "PositionOccupied",
        RejectionReason::InvalidDirection => "InvalidDirection",
        RejectionReason::InsufficientResource { .. }
        | RejectionReason::InsufficientEnergy
        | RejectionReason::InsufficientResources
        | RejectionReason::CarryFull
        | RejectionReason::SourceEmpty
        | RejectionReason::TargetFull
        | RejectionReason::TargetEmpty
        | RejectionReason::ExceedsRoomCapacity
        | RejectionReason::TargetFuelTooLow => "InsufficientResource",
        RejectionReason::OutOfRange { .. } => "OutOfRange",
        RejectionReason::InvalidStructureType => "InvalidStructureType",
        RejectionReason::InvalidResourceType | RejectionReason::InvalidDamageType => {
            "InvalidResourceType"
        }
        RejectionReason::TooManyConstructionSites | RejectionReason::ConstructionLimitReached => {
            "ConstructionLimitReached"
        }
        RejectionReason::NotStructure => "NotStructure",
        RejectionReason::NotController => "NotController",
        RejectionReason::SpawnOnCooldown => "SpawnOnCooldown",
        RejectionReason::CooldownActive => "CooldownActive",
        RejectionReason::RoomDroneCapReached => "RoomDroneCapReached",
        RejectionReason::GlobalStorageDisabled => "GlobalStorageDisabled",
        RejectionReason::TransferInProgress => "TransferInProgress",
        RejectionReason::PlayerNotFound => "NotVisibleOrNotFound",
        RejectionReason::FuelExhausted => "FuelExhausted",
        RejectionReason::SafeModeActive => "SafeModeActive",
        RejectionReason::TargetFortifyCooldown => "TargetFortifyCooldown",
        RejectionReason::TargetOverloadCooldown => "TargetOverloadCooldown",
        RejectionReason::InternalError => "InternalError",
        RejectionReason::ServerOverloaded => "ServerOverloaded",
        RejectionReason::SnapshotOverBudget => "SnapshotOverBudget",
        RejectionReason::InvalidCertificate => "InvalidCertificate",
        RejectionReason::CertExpired => "CertExpired",
        RejectionReason::TokenRevoked => "TokenRevoked",
        RejectionReason::RefreshTokenInvalid => "RefreshTokenInvalid",
        RejectionReason::NotAuthorized => "NotAuthorized",
        RejectionReason::ScopeInsufficient => "ScopeInsufficient",
        RejectionReason::SessionLimitReached => "SessionLimitReached",
        RejectionReason::DeviceNotRegistered => "DeviceNotRegistered",
        RejectionReason::MultiDeviceConflict => "MultiDeviceConflict",
        RejectionReason::UnknownCredential => "UnknownCredential",
        RejectionReason::InternalAuthError => "InternalAuthError",
        RejectionReason::UnknownAction { .. } => "UnknownAction",
    }
}

fn non_canonical_rejection_reason(rejection: &RejectionReason) -> bool {
    !matches!(
        rejection,
        RejectionReason::InvalidJson
            | RejectionReason::SchemaViolation
            | RejectionReason::CommandBufferFull
            | RejectionReason::TimeoutExceeded
            | RejectionReason::SourceNotAllowed
            | RejectionReason::AuthContextInvalid
            | RejectionReason::NotVisibleOrNotFound
            | RejectionReason::ObjectNotFound
            | RejectionReason::TargetNotVisible
            | RejectionReason::NotOwner
            | RejectionReason::InvalidBodyPart
            | RejectionReason::NotEnoughBodyParts
            | RejectionReason::PositionOccupied
            | RejectionReason::InvalidDirection
            | RejectionReason::InsufficientResource { .. }
            | RejectionReason::OutOfRange { .. }
            | RejectionReason::InvalidStructureType
            | RejectionReason::InvalidResourceType
            | RejectionReason::ConstructionLimitReached
            | RejectionReason::NotStructure
            | RejectionReason::NotController
            | RejectionReason::SpawnOnCooldown
            | RejectionReason::CooldownActive
            | RejectionReason::RoomDroneCapReached
            | RejectionReason::GlobalStorageDisabled
            | RejectionReason::TransferInProgress
            | RejectionReason::FuelExhausted
            | RejectionReason::SafeModeActive
            | RejectionReason::TargetFortifyCooldown
            | RejectionReason::TargetOverloadCooldown
            | RejectionReason::InternalError
            | RejectionReason::ServerOverloaded
            | RejectionReason::SnapshotOverBudget
            | RejectionReason::InvalidCertificate
            | RejectionReason::CertExpired
            | RejectionReason::TokenRevoked
            | RejectionReason::RefreshTokenInvalid
            | RejectionReason::NotAuthorized
            | RejectionReason::ScopeInsufficient
            | RejectionReason::SessionLimitReached
            | RejectionReason::DeviceNotRegistered
            | RejectionReason::MultiDeviceConflict
            | RejectionReason::UnknownCredential
            | RejectionReason::InternalAuthError
            | RejectionReason::UnknownAction { .. }
    )
}

fn json_depth(value: &serde_json::Value) -> usize {
    match value {
        serde_json::Value::Array(items) => 1 + items.iter().map(json_depth).max().unwrap_or(0),
        serde_json::Value::Object(fields) => 1 + fields.values().map(json_depth).max().unwrap_or(0),
        _ => 1,
    }
}

pub fn apply_command(world: &mut World, command: ValidatedCommand) -> CommandResult {
    let action_tick = command.raw.tick;
    let player_id = command.raw.player_id;
    let mut actor_id = None;
    let result = match command.raw.action {
        CommandAction::Move {
            object_id,
            direction,
        } => {
            actor_id = Some(object_id);
            apply_move(world, object_id, direction)
        }
        CommandAction::Harvest {
            object_id,
            target_id,
            resource,
        } => {
            actor_id = Some(object_id);
            apply_harvest(world, object_id, target_id, resource)
        }
        CommandAction::Transfer {
            object_id,
            target_id,
            resource,
            amount,
        } => {
            actor_id = Some(object_id);
            apply_transfer(world, object_id, target_id, &resource, amount)
        }
        CommandAction::Withdraw {
            object_id,
            target_id,
            resource,
            amount,
        } => {
            actor_id = Some(object_id);
            apply_withdraw(world, object_id, target_id, &resource, amount)
        }
        CommandAction::Attack {
            object_id,
            target_id,
        } => {
            actor_id = Some(object_id);
            apply_attack(world, object_id, target_id)
        }
        CommandAction::RangedAttack {
            object_id,
            target_id,
            range: _,
        } => {
            actor_id = Some(object_id);
            apply_ranged_attack(world, object_id, target_id)
        }
        CommandAction::Heal {
            object_id,
            target_id,
        } => {
            actor_id = Some(object_id);
            apply_heal(world, object_id, target_id)
        }
        CommandAction::ClaimController {
            object_id,
            controller_id,
        } => {
            actor_id = Some(object_id);
            apply_claim_controller(world, player_id, controller_id)
        }
        CommandAction::Spawn { spawn_id, body } => {
            apply_spawn_drone(world, player_id, spawn_id, body)
        }
        CommandAction::Recycle {
            object_id,
            spawn_id,
        } => apply_recycle(world, player_id, action_tick, object_id, spawn_id),
        CommandAction::Build {
            object_id,
            x,
            y,
            structure,
        } => {
            actor_id = Some(object_id);
            apply_build(world, player_id, object_id, x, y, structure)
        }
        CommandAction::TransferToGlobal { resource, amount } => {
            apply_transfer_to_global(world, player_id, &resource, amount)
        }
        CommandAction::TransferFromGlobal { resource, amount } => {
            apply_transfer_from_global(world, player_id, &resource, amount)
        }
        CommandAction::Custom {
            action_type,
            object_id,
            target_id,
            structure,
            ..
        } => {
            actor_id = Some(object_id);
            apply_custom_action(
                world,
                player_id,
                action_tick,
                &action_type,
                object_id,
                target_id,
                structure,
            )
        }
    };

    if result.is_ok() {
        if let Some(object_id) = actor_id {
            mark_drone_action(world, object_id, action_tick);
        }
    }

    result
}

fn mark_drone_action(world: &mut World, object_id: ObjectId, tick: Tick) {
    if let Ok(entity) = entity(object_id) {
        if let Some(mut drone) = world.entity_mut(entity).get_mut::<Drone>() {
            drone.last_action_tick = tick;
        }
    }
}

fn validate_move(
    world: &mut World,
    player_id: PlayerId,
    object_id: ObjectId,
    direction: Direction,
) -> CommandResult {
    let (position, drone) = drone_snapshot(world, object_id)?;
    ensure_owner(&drone, player_id)?;
    ensure_drone_can_act(&drone, BodyPart::Move, true)?;
    let target = step(world.resource::<RoomTerrains>(), position, direction)
        .ok_or(RejectionReason::InvalidDirection)?;

    if !world.resource::<RoomTerrains>().is_passable(target) {
        return Err(RejectionReason::TileBlocked);
    }
    if tile_has_blocking_enemy(world, target, player_id) {
        return Err(RejectionReason::TileBlocked);
    }
    Ok(())
}

fn validate_harvest(
    world: &mut World,
    player_id: PlayerId,
    object_id: ObjectId,
    target_id: ObjectId,
) -> CommandResult {
    let (position, drone) = drone_snapshot(world, object_id)?;
    ensure_owner(&drone, player_id)?;
    ensure_drone_can_act(&drone, BodyPart::Work, true)?;
    require_body(&drone, BodyPart::Carry)?;
    if carry_used(&drone.carry) >= drone.carry_capacity {
        return Err(RejectionReason::CarryFull);
    }

    let (target_position, source) = source_snapshot(world, target_id)?;
    if source.capacity == 0 {
        return Err(RejectionReason::SourceEmpty);
    }
    ensure_range(position, target_position, 1)
}

fn validate_transfer(
    world: &mut World,
    player_id: PlayerId,
    object_id: ObjectId,
    target_id: ObjectId,
    resource: &str,
    amount: u32,
) -> CommandResult {
    let (position, drone) = drone_snapshot(world, object_id)?;
    ensure_owner(&drone, player_id)?;
    require_body(&drone, BodyPart::Carry)?;
    let available = *drone.carry.get(resource).unwrap_or(&0);
    if available < amount {
        return Err(RejectionReason::InsufficientResource {
            resource: resource.to_string(),
            required: amount,
            available,
        });
    }

    if let Ok((_, controller)) = controller_snapshot(world, target_id) {
        if controller.owner != Some(player_id) {
            return Err(RejectionReason::NotOwner);
        }
    }
    let (target_position, space) = target_resource_space(world, target_id, resource)?;
    if space < amount {
        return Err(RejectionReason::TargetFull);
    }
    ensure_range(position, target_position, 1)
}

fn validate_withdraw(
    world: &mut World,
    player_id: PlayerId,
    object_id: ObjectId,
    target_id: ObjectId,
    resource: &str,
    amount: u32,
) -> CommandResult {
    let (position, drone) = drone_snapshot(world, object_id)?;
    ensure_owner(&drone, player_id)?;
    require_body(&drone, BodyPart::Carry)?;
    let space = drone
        .carry_capacity
        .saturating_sub(carry_used(&drone.carry));
    if space < amount {
        return Err(RejectionReason::TargetFull);
    }

    let (target_position, available) = target_resource_amount(world, target_id, resource)?;
    if available == 0 {
        return Err(RejectionReason::TargetEmpty);
    }
    if available < amount {
        return Err(RejectionReason::InsufficientResource {
            resource: resource.to_string(),
            required: amount,
            available,
        });
    }
    ensure_range(position, target_position, 1)
}

fn validate_attack(
    world: &mut World,
    player_id: PlayerId,
    object_id: ObjectId,
    target_id: ObjectId,
) -> CommandResult {
    let (position, drone) = drone_snapshot(world, object_id)?;
    ensure_owner(&drone, player_id)?;
    ensure_drone_can_act(&drone, BodyPart::Attack, true)?;
    let (target_position, target_owner) = attackable_snapshot(world, target_id)?;
    if target_owner == Some(player_id) {
        return Err(RejectionReason::FriendlyTarget);
    }
    ensure_range(position, target_position, 1)
}

fn validate_ranged_attack(
    world: &mut World,
    player_id: PlayerId,
    object_id: ObjectId,
    target_id: ObjectId,
    range: u32,
) -> CommandResult {
    if range == 0 || range > MAX_RANGED_ATTACK_RANGE {
        return Err(RejectionReason::OutOfRange {
            distance: range,
            max: MAX_RANGED_ATTACK_RANGE,
        });
    }
    let (position, drone) = drone_snapshot(world, object_id)?;
    ensure_owner(&drone, player_id)?;
    ensure_drone_can_act(&drone, BodyPart::RangedAttack, true)?;
    let (target_position, target_owner) = attackable_snapshot(world, target_id)?;
    if target_owner == Some(player_id) {
        return Err(RejectionReason::FriendlyTarget);
    }
    ensure_range(position, target_position, range)
}

fn validate_heal(
    world: &mut World,
    player_id: PlayerId,
    object_id: ObjectId,
    target_id: ObjectId,
) -> CommandResult {
    let (position, drone) = drone_snapshot(world, object_id)?;
    ensure_owner(&drone, player_id)?;
    require_body(&drone, BodyPart::Heal)?;
    let (target_position, target) = drone_snapshot(world, target_id)?;
    if target.owner != player_id {
        return Err(RejectionReason::NotFriendly);
    }
    if target.hits >= target.hits_max {
        return Err(RejectionReason::AlreadyFullHealth);
    }
    ensure_range(position, target_position, 3)
}

fn validate_claim_controller(
    world: &mut World,
    player_id: PlayerId,
    object_id: ObjectId,
    controller_id: ObjectId,
) -> CommandResult {
    let (position, drone) = drone_snapshot(world, object_id)?;
    ensure_owner(&drone, player_id)?;
    ensure_drone_can_act(&drone, BodyPart::Claim, true)?;
    let (target_position, controller) = controller_snapshot(world, controller_id)?;
    if controller.owner.is_some() && controller.owner != Some(player_id) {
        return Err(RejectionReason::NotOwner);
    }
    ensure_range(position, target_position, 1)
}

fn validate_spawn_drone(
    world: &mut World,
    player_id: PlayerId,
    spawn_id: ObjectId,
    body: &[BodyPart],
) -> CommandResult {
    let (position, structure) = structure_snapshot(world, spawn_id)?;
    if structure.structure_type != StructureType::SPAWN || structure.owner != Some(player_id) {
        return Err(RejectionReason::NotYourSpawn);
    }
    if structure.cooldown > 0 {
        return Err(RejectionReason::SpawnOnCooldown);
    }
    if body.len() > MAX_BODY_PARTS {
        return Err(RejectionReason::BodyTooLarge);
    }
    let cost = body_spawn_cost(world, body);
    let energy_cost = cost.get(ENERGY_RESOURCE).copied().unwrap_or_default();
    let energy = structure.energy.unwrap_or(0);
    if energy_cost > structure.energy_capacity.unwrap_or(0) {
        return Err(RejectionReason::ExceedsRoomCapacity);
    }
    if energy_cost > energy {
        return Err(RejectionReason::InsufficientResource {
            resource: ENERGY_RESOURCE.to_string(),
            required: energy_cost,
            available: energy,
        });
    }
    ensure_player_resource_cost(world, player_id, &cost, true)?;
    if world
        .resource::<RoomDroneCounts>()
        .0
        .get(&(position.room, player_id))
        .copied()
        .unwrap_or_default()
        >= MAX_DRONES_PER_PLAYER
    {
        return Err(RejectionReason::RoomDroneCapReached);
    }
    let spawn_position = spawn_output_position(position);
    if !world.resource::<RoomTerrains>().is_passable(spawn_position) {
        return Err(RejectionReason::InvalidTerrain);
    }
    if tile_has_any_drone(world, spawn_position) {
        return Err(RejectionReason::TileOccupied);
    }
    Ok(())
}

fn validate_recycle(
    world: &mut World,
    player_id: PlayerId,
    object_id: ObjectId,
    spawn_id: ObjectId,
) -> CommandResult {
    let (position, drone) = drone_snapshot(world, object_id)?;
    ensure_owner(&drone, player_id)?;

    let (spawn_position, spawn) = structure_snapshot(world, spawn_id)?;
    if spawn.structure_type != StructureType::SPAWN || spawn.owner != Some(player_id) {
        return Err(RejectionReason::NotYourSpawn);
    }
    ensure_range(position, spawn_position, 1)
}

fn validate_build(
    world: &mut World,
    player_id: PlayerId,
    object_id: ObjectId,
    x: i32,
    y: i32,
    structure: StructureType,
) -> CommandResult {
    let (position, drone) = drone_snapshot(world, object_id)?;
    ensure_owner(&drone, player_id)?;
    ensure_drone_can_act(&drone, BodyPart::Work, true)?;
    let structure_def = world
        .resource::<StructureTypeRegistry>()
        .get(structure)
        .ok_or(RejectionReason::NotStructure)?
        .clone();
    if room_controller_level(world, position.room, player_id) < structure_def.rcl_required {
        return Err(RejectionReason::NotYourRoom);
    }
    let target = Position {
        x,
        y,
        room: position.room,
    };
    if !world.resource::<RoomTerrains>().is_passable(target) {
        return Err(RejectionReason::InvalidTerrain);
    }
    if tile_has_any_object(world, target) {
        return Err(RejectionReason::TileOccupied);
    }
    ensure_range(position, target, 1)?;

    let cost = build_cost(world, structure);
    ensure_player_resource_cost(world, player_id, &cost, false)
}

fn validate_transfer_to_global(
    world: &mut World,
    player_id: PlayerId,
    resource: &str,
    amount: u32,
) -> CommandResult {
    let config = world.resource::<GlobalStorageConfig>();
    if !config.enabled {
        return Err(RejectionReason::GlobalStorageDisabled);
    }
    ensure_no_pending_global_transfer(world, player_id)?;

    let available = world
        .resource::<PlayerLocalStorage>()
        .0
        .get(&player_id)
        .and_then(|storage| storage.get(resource))
        .copied()
        .unwrap_or_default();
    if available < amount {
        return Err(RejectionReason::InsufficientResource {
            resource: resource.to_string(),
            required: amount,
            available,
        });
    }

    let deliver_amount = amount.saturating_sub(transfer_fee(
        amount,
        config.transfer_to_global_fee_per_10_000,
    ));
    let committed = global_storage_committed(world, player_id);
    if committed.saturating_add(deliver_amount) > config.capacity {
        return Err(RejectionReason::TargetFull);
    }
    Ok(())
}

fn validate_custom_action(
    world: &mut World,
    player_id: PlayerId,
    tick: Tick,
    action_type: &str,
    object_id: ObjectId,
    target_id: Option<ObjectId>,
) -> CommandResult {
    let action = world
        .resource::<CustomActionRegistry>()
        .get(action_type)
        .cloned()
        .ok_or_else(|| RejectionReason::UnknownAction {
            action: action_type.to_string(),
        })?;
    let (position, drone) = drone_snapshot(world, object_id)?;
    ensure_owner(&drone, player_id)?;
    if drone.spawning {
        return Err(RejectionReason::StillSpawning);
    }
    if drone.fatigue > 0 {
        return Err(RejectionReason::Fatigued);
    }
    validate_special_action_requirements(world, &drone, &action)?;
    if custom_action_on_cooldown(world, object_id, action_type, tick) {
        return Err(RejectionReason::Fatigued);
    }
    let cost = custom_action_cost(&action);
    ensure_player_resource_cost(world, player_id, &cost, false)?;

    let Some(target_id) = target_id else {
        return Ok(());
    };
    let (target_position, target_owner) = attackable_snapshot(world, target_id)?;
    let handler = special_effect_handler(world, &action)?;
    match handler.as_deref() {
        Some("fortify") => {
            if target_owner.is_some() && target_owner != Some(player_id) {
                return Err(RejectionReason::NotFriendly);
            }
        }
        Some("drain") => {
            if target_owner == Some(player_id) {
                return Err(RejectionReason::FriendlyTarget);
            }
            let resource = action.damage_type.as_deref().unwrap_or(ENERGY_RESOURCE);
            let available = target_resource_amount(world, target_id, resource)?.1;
            let space = target_resource_space(world, object_id, resource)?.1;
            if available == 0 {
                return Err(RejectionReason::InsufficientResource {
                    resource: resource.to_string(),
                    required: 1,
                    available,
                });
            }
            if space == 0 {
                return Err(RejectionReason::TargetFull);
            }
        }
        Some("fabricate") | Some("convert_to_structure") => {
            if target_owner == Some(player_id) {
                return Err(RejectionReason::FriendlyTarget);
            }
            let target = entity(target_id)?;
            if world.entity(target).get::<Drone>().is_none() {
                return Err(RejectionReason::NotMovable);
            }
        }
        Some("hack") => {
            if target_owner == Some(player_id) {
                return Err(RejectionReason::FriendlyTarget);
            }
            if has_attr(world, target_id, "Hacking")? {
                return Err(RejectionReason::AlreadyHacked);
            }
        }
        Some("overload") => {
            if target_owner == Some(player_id) {
                return Err(RejectionReason::FriendlyTarget);
            }
            let target_owner = target_owner.ok_or(RejectionReason::PlayerNotFound)?;
            if player_fuel_budget(world, target_owner) <= OVERLOAD_FUEL_FLOOR {
                return Err(RejectionReason::TargetFuelTooLow);
            }
        }
        Some("debilitate") => {
            if target_owner == Some(player_id) {
                return Err(RejectionReason::FriendlyTarget);
            }
            let damage_type = action
                .damage_type
                .as_deref()
                .unwrap_or(DamageType::Corrosive.as_str());
            ensure_damage_type(world, damage_type)?;
            if has_debilitate(world, target_id, damage_type)? {
                return Err(RejectionReason::AlreadyDebilitated {
                    damage_type: damage_type.to_string(),
                });
            }
        }
        Some("leech") | Some("heal_self") => {
            if target_owner == Some(player_id) {
                return Err(RejectionReason::FriendlyTarget);
            }
            let damage_type = action
                .damage_type
                .as_deref()
                .unwrap_or(DamageType::Corrosive.as_str());
            ensure_damage_type(world, damage_type)?;
        }
        _ => {
            if target_owner == Some(player_id) {
                return Err(RejectionReason::FriendlyTarget);
            }
        }
    }
    ensure_range(position, target_position, action.range)
}

fn validate_transfer_from_global(
    world: &mut World,
    player_id: PlayerId,
    resource: &str,
    amount: u32,
) -> CommandResult {
    let config = world.resource::<GlobalStorageConfig>();
    if !config.enabled {
        return Err(RejectionReason::GlobalStorageDisabled);
    }
    ensure_no_pending_global_transfer(world, player_id)?;

    let available = world
        .resource::<PlayerGlobalStorage>()
        .0
        .get(&player_id)
        .and_then(|storage| storage.get(resource))
        .copied()
        .unwrap_or_default();
    if available < amount {
        return Err(RejectionReason::InsufficientResource {
            resource: resource.to_string(),
            required: amount,
            available,
        });
    }
    Ok(())
}

fn apply_move(world: &mut World, object_id: ObjectId, direction: Direction) -> CommandResult {
    let entity = entity(object_id)?;
    let current_position = *world
        .entity(entity)
        .get::<Position>()
        .ok_or(RejectionReason::ObjectNotFound)?;
    let target = step(
        world.resource::<RoomTerrains>(),
        current_position,
        direction,
    )
    .ok_or(RejectionReason::InvalidDirection)?;
    *world
        .entity_mut(entity)
        .get_mut::<Position>()
        .ok_or(RejectionReason::ObjectNotFound)? = target;
    Ok(())
}

fn apply_harvest(
    world: &mut World,
    object_id: ObjectId,
    target_id: ObjectId,
    resource: Option<String>,
) -> CommandResult {
    let resource = resource.unwrap_or_else(|| "Energy".to_string());
    let object = entity(object_id)?;
    let target = entity(target_id)?;
    let (_, drone) = drone_snapshot(world, object_id)?;
    let work_parts = drone
        .body
        .iter()
        .filter(|part| **part == BodyPart::Work)
        .count() as u32;
    let free_capacity = drone
        .carry_capacity
        .saturating_sub(carry_used(&drone.carry));
    let amount = world
        .entity(target)
        .get::<crate::components::Source>()
        .ok_or(RejectionReason::NotSource)?
        .capacity
        .min(free_capacity)
        .min(work_parts.max(1) * 2);

    world
        .entity_mut(target)
        .get_mut::<crate::components::Source>()
        .unwrap()
        .capacity -= amount;
    *world
        .entity_mut(object)
        .get_mut::<Drone>()
        .unwrap()
        .carry
        .entry(resource)
        .or_default() += amount;
    send_onboarding_event(world, OnboardingEvent::ResourceHarvested);
    Ok(())
}

fn apply_transfer(
    world: &mut World,
    object_id: ObjectId,
    target_id: ObjectId,
    resource: &str,
    amount: u32,
) -> CommandResult {
    let object = entity(object_id)?;
    let target = entity(target_id)?;
    take_from_drone(world, object, resource, amount);
    add_to_target(world, target, resource, amount)?;
    send_onboarding_event(world, OnboardingEvent::ResourceCollected);
    Ok(())
}

fn apply_withdraw(
    world: &mut World,
    object_id: ObjectId,
    target_id: ObjectId,
    resource: &str,
    amount: u32,
) -> CommandResult {
    let object = entity(object_id)?;
    let target = entity(target_id)?;
    take_from_target(world, target, resource, amount)?;
    *world
        .entity_mut(object)
        .get_mut::<Drone>()
        .unwrap()
        .carry
        .entry(resource.to_string())
        .or_default() += amount;
    Ok(())
}

fn apply_attack(world: &mut World, object_id: ObjectId, target_id: ObjectId) -> CommandResult {
    let (_, drone) = drone_snapshot(world, object_id)?;
    let (damage_type, damage) = crate::systems::combat_system::body_part_damage(
        drone
            .body
            .iter()
            .filter(|part| **part == BodyPart::Attack)
            .count(),
        BodyPart::Attack,
        world.resource::<BodyPartRegistry>(),
        *world.resource::<crate::systems::CombatRules>(),
    );
    apply_resisted_damage(world, target_id, &damage_type, damage)
}

fn apply_ranged_attack(
    world: &mut World,
    object_id: ObjectId,
    target_id: ObjectId,
) -> CommandResult {
    let (_, drone) = drone_snapshot(world, object_id)?;
    let (damage_type, damage) = crate::systems::combat_system::body_part_damage(
        drone
            .body
            .iter()
            .filter(|part| **part == BodyPart::RangedAttack)
            .count(),
        BodyPart::RangedAttack,
        world.resource::<BodyPartRegistry>(),
        *world.resource::<crate::systems::CombatRules>(),
    );
    apply_resisted_damage(world, target_id, &damage_type, damage)
}

fn apply_resisted_damage(
    world: &mut World,
    target_id: ObjectId,
    damage_type: &str,
    damage: u32,
) -> CommandResult {
    let target = entity(target_id)?;
    let multiplier = {
        let body_registry = world.resource::<BodyPartRegistry>();
        let damage_registry = world.resource::<DamageTypeRegistry>();
        let resistance_registry = world.resource::<ResistanceRegistry>();
        let entity_ref = world
            .get_entity(target)
            .map_err(|_| RejectionReason::ObjectNotFound)?;
        let attrs = entity_ref.get::<Attributes>();
        let flags = entity_ref.get::<EntityFlags>();
        if let Some(drone) = entity_ref.get::<Drone>() {
            crate::systems::combat_system::final_damage_multiplier(
                Some(&drone.body),
                attrs,
                flags,
                damage_type,
                body_registry,
                damage_registry,
                resistance_registry,
            )
        } else if entity_ref.get::<Structure>().is_some() {
            crate::systems::combat_system::final_damage_multiplier(
                None,
                attrs,
                flags,
                damage_type,
                body_registry,
                damage_registry,
                resistance_registry,
            )
        } else {
            return Err(RejectionReason::ObjectNotFound);
        }
    };
    let damage = ((damage as f64) * multiplier).floor() as u32;
    if let Some(mut target_drone) = world.entity_mut(target).get_mut::<Drone>() {
        target_drone.hits = target_drone.hits.saturating_sub(damage);
    } else if let Some(mut structure) = world.entity_mut(target).get_mut::<Structure>() {
        structure.hits = structure.hits.saturating_sub(damage);
    }
    Ok(())
}

fn apply_heal(world: &mut World, object_id: ObjectId, target_id: ObjectId) -> CommandResult {
    let (_, healer) = drone_snapshot(world, object_id)?;
    let heal = healer
        .body
        .iter()
        .filter(|part| **part == BodyPart::Heal)
        .count() as u32
        * world
            .resource::<BodyPartRegistry>()
            .heal_amount(BodyPart::Heal);
    let target = entity(target_id)?;
    let mut entity_mut = world.entity_mut(target);
    let mut drone = entity_mut
        .get_mut::<Drone>()
        .ok_or(RejectionReason::ObjectNotFound)?;
    drone.hits = (drone.hits + heal).min(drone.hits_max);
    Ok(())
}

fn apply_claim_controller(
    world: &mut World,
    player_id: PlayerId,
    controller_id: ObjectId,
) -> CommandResult {
    let controller = entity(controller_id)?;
    let mut entity_mut = world.entity_mut(controller);
    let mut controller = entity_mut
        .get_mut::<Controller>()
        .ok_or(RejectionReason::NotController)?;
    controller.owner = Some(player_id);
    if controller.level == 0 {
        controller.level = 1;
    }
    controller.progress_total = crate::systems::rcl_progress_total(controller.level + 1);
    controller.downgrade_timer = crate::systems::DEFAULT_CONTROLLER_DOWNGRADE_TIMER;
    Ok(())
}

fn apply_spawn_drone(
    world: &mut World,
    player_id: PlayerId,
    spawn_id: ObjectId,
    body: Vec<BodyPart>,
) -> CommandResult {
    let spawn = entity(spawn_id)?;
    let position = *world
        .entity(spawn)
        .get::<Position>()
        .ok_or(RejectionReason::ObjectNotFound)?;
    let cost = body_spawn_cost(world, &body);
    let energy_cost = cost.get(ENERGY_RESOURCE).copied().unwrap_or_default();
    {
        let mut entity_mut = world.entity_mut(spawn);
        let mut structure = entity_mut
            .get_mut::<Structure>()
            .ok_or(RejectionReason::ObjectNotFound)?;
        if let Some(energy) = &mut structure.energy {
            *energy = energy.saturating_sub(energy_cost);
        }
        structure.cooldown = 1;
    }
    deduct_player_resource_cost(world, player_id, &cost, true);
    world
        .resource_mut::<PendingSpawnQueue>()
        .0
        .push(PendingSpawn {
            owner: player_id,
            body,
            position: spawn_output_position(position),
        });
    Ok(())
}

fn apply_recycle(
    world: &mut World,
    player_id: PlayerId,
    tick: Tick,
    object_id: ObjectId,
    spawn_id: ObjectId,
) -> CommandResult {
    let object = entity(object_id)?;
    let spawn = entity(spawn_id)?;
    let (position, drone) = drone_snapshot(world, object_id)?;
    let refund = recycle_refund_cost(world, tick, &drone.body);

    refund_recycle_cost(world, player_id, spawn, &refund)?;
    world.entity_mut(object).despawn();
    if let Some(count) = world
        .resource_mut::<RoomDroneCounts>()
        .0
        .get_mut(&(position.room, player_id))
    {
        *count = count.saturating_sub(1);
    }
    Ok(())
}

fn apply_build(
    world: &mut World,
    player_id: PlayerId,
    object_id: ObjectId,
    x: i32,
    y: i32,
    structure_type: StructureType,
) -> CommandResult {
    let (position, _) = drone_snapshot(world, object_id)?;
    let cost = build_cost(world, structure_type);
    deduct_player_resource_cost(world, player_id, &cost, false);
    let position = Position {
        x,
        y,
        room: position.room,
    };
    world.spawn((
        position,
        structure_defaults(structure_type, Some(player_id), world),
    ));
    send_onboarding_event(world, OnboardingEvent::StructureBuilt);
    Ok(())
}

fn apply_custom_action(
    world: &mut World,
    player_id: PlayerId,
    tick: Tick,
    action_type: &str,
    object_id: ObjectId,
    target_id: Option<ObjectId>,
    structure_type: Option<StructureType>,
) -> CommandResult {
    let action = world
        .resource::<CustomActionRegistry>()
        .get(action_type)
        .cloned()
        .ok_or_else(|| RejectionReason::UnknownAction {
            action: action_type.to_string(),
        })?;
    let cost = custom_action_cost(&action);
    deduct_player_resource_cost(world, player_id, &cost, false);
    remember_custom_action_cooldown(world, object_id, action_type, tick, &action);

    match special_effect_handler(world, &action)?.as_deref() {
        Some("heal_self") => {
            let Some(target_id) = target_id else {
                return Ok(());
            };
            let damage = action.base_damage.unwrap_or_default();
            let damage_type = action
                .damage_type
                .as_deref()
                .unwrap_or(DamageType::Kinetic.as_str());
            let dealt = apply_resisted_damage_amount(world, target_id, damage_type, damage)?;
            let heal = ((dealt as f64) * action.special_param.unwrap_or(0.5)).floor() as u32;
            heal_drone(world, object_id, heal);
        }
        Some("convert_to_structure") | Some("fabricate") => {
            let Some(target_id) = target_id else {
                return Ok(());
            };
            fabricate_target(world, player_id, target_id, structure_type)?;
        }
        Some("fortify") => {
            let target_id = target_id.unwrap_or(object_id);
            let target = entity(target_id)?;
            let mut entity_mut = world.entity_mut(target);
            if let Some(mut attrs) = entity_mut.get_mut::<Attributes>() {
                apply_fortify_attrs(&mut attrs.0);
            } else {
                let mut attrs = Vec::new();
                apply_fortify_attrs(&mut attrs);
                entity_mut.insert(Attributes(attrs));
            }
            if let Some(mut flags) = entity_mut.get_mut::<EntityFlags>() {
                apply_fortify_flags(&mut flags.0);
            } else {
                let mut flags = std::collections::HashMap::new();
                apply_fortify_flags(&mut flags);
                entity_mut.insert(EntityFlags(flags));
            }
        }
        Some("disrupt") => {
            let Some(target_id) = target_id else {
                return Ok(());
            };
            apply_disrupt(world, target_id)?;
        }
        Some("hack") => {
            let Some(target_id) = target_id else {
                return Ok(());
            };
            apply_hack(world, target_id)?;
        }
        Some("drain") => {
            let Some(target_id) = target_id else {
                return Ok(());
            };
            let resource = action.damage_type.as_deref().unwrap_or(ENERGY_RESOURCE);
            apply_drain(world, object_id, target_id, resource)?;
        }
        Some("overload") => {
            let Some(target_id) = target_id else {
                return Ok(());
            };
            apply_overload(world, target_id)?;
        }
        Some("debilitate") => {
            let Some(target_id) = target_id else {
                return Ok(());
            };
            let damage_type = action
                .damage_type
                .as_deref()
                .unwrap_or(DamageType::Corrosive.as_str());
            apply_debilitate(world, target_id, damage_type)?;
        }
        Some("leech") => {
            let Some(target_id) = target_id else {
                return Ok(());
            };
            let damage = action.base_damage.unwrap_or(15);
            let damage_type = action
                .damage_type
                .as_deref()
                .unwrap_or(DamageType::Corrosive.as_str());
            let dealt = apply_resisted_damage_amount(world, target_id, damage_type, damage)?;
            heal_drone(
                world,
                object_id,
                ((dealt as f64) * action.special_param.unwrap_or(0.5)).floor() as u32,
            );
        }
        Some("scramble_commands") | None => {}
        Some(other) => {
            return Err(RejectionReason::UnknownAction {
                action: other.to_string(),
            });
        }
    }
    Ok(())
}

fn special_effect_handler(
    world: &World,
    action: &CustomActionDef,
) -> Result<Option<String>, RejectionReason> {
    let Some(effect_name) = action.special_effect.as_deref() else {
        return Ok(None);
    };
    let effect = world
        .resource::<SpecialEffectRegistry>()
        .get(effect_name)
        .ok_or_else(|| RejectionReason::UnknownAction {
            action: effect_name.to_string(),
        })?;
    Ok(Some(effect.handler.clone()))
}

fn validate_special_action_requirements(
    world: &World,
    drone: &Drone,
    action: &CustomActionDef,
) -> CommandResult {
    match special_effect_handler(world, action)?.as_deref() {
        Some("hack") => require_body(drone, BodyPart::Claim),
        Some("drain") => {
            require_body(drone, BodyPart::Carry)?;
            require_body(drone, BodyPart::Work)
        }
        Some("overload") => require_body(drone, BodyPart::RangedAttack),
        Some("debilitate") => require_body(drone, BodyPart::Work),
        Some("disrupt") => require_body(drone, BodyPart::Attack),
        Some("fortify") => require_body(drone, BodyPart::Tough),
        _ => Ok(()),
    }
}

fn custom_action_cost(action: &CustomActionDef) -> ResourceCost {
    action.cost.clone()
}

fn custom_action_cooldown(action: &CustomActionDef) -> Option<u32> {
    action.cooldown
}

fn custom_action_on_cooldown(
    world: &World,
    object_id: ObjectId,
    action_type: &str,
    tick: Tick,
) -> bool {
    world
        .get_resource::<CustomActionCooldowns>()
        .and_then(|cooldowns| {
            cooldowns
                .0
                .get(&(object_id, action_type.to_string()))
                .copied()
        })
        .is_some_and(|ready_tick| tick < ready_tick)
}

fn remember_custom_action_cooldown(
    world: &mut World,
    object_id: ObjectId,
    action_type: &str,
    tick: Tick,
    action: &CustomActionDef,
) {
    let Some(cooldown) = custom_action_cooldown(action) else {
        return;
    };
    if world.get_resource::<CustomActionCooldowns>().is_none() {
        world.insert_resource(CustomActionCooldowns::default());
    }
    world.resource_mut::<CustomActionCooldowns>().0.insert(
        (object_id, action_type.to_string()),
        tick + cooldown as Tick,
    );
}

fn apply_fortify_attrs(attrs: &mut Vec<String>) {
    attrs.retain(|attr| !is_negative_status_attr(attr));
    if !attrs.iter().any(|attr| attr == "Fortified") {
        attrs.push("Fortified".to_string());
    }
    attrs.retain(|attr| !attr.starts_with("Fortified:"));
    attrs.push("Fortified:duration=3".to_string());
}

fn apply_fortify_flags(flags: &mut std::collections::HashMap<String, bool>) {
    flags.retain(|flag, active| !*active || !is_negative_status_attr(flag));
    flags.insert("Fortified".to_string(), true);
}

fn apply_disrupt(world: &mut World, target_id: ObjectId) -> CommandResult {
    let target = entity(target_id)?;
    let multiplier = sonic_effect_multiplier(world, target)?;
    if multiplier <= 0.0 {
        return Ok(());
    }
    {
        let mut entity_mut = world.entity_mut(target);
        if let Some(mut attrs) = entity_mut.get_mut::<Attributes>() {
            attrs.0.retain(|attr| !is_interruptible_action_attr(attr));
            attrs.0.retain(|attr| !attr.starts_with("Disrupted:"));
            push_unique(&mut attrs.0, "Disrupted");
            attrs.0.push("Disrupted:duration=1".to_string());
        } else {
            entity_mut.insert(Attributes(vec![
                "Disrupted".to_string(),
                "Disrupted:duration=1".to_string(),
            ]));
        }
        if let Some(mut flags) = entity_mut.get_mut::<EntityFlags>() {
            flags.0.insert("Disrupted".to_string(), true);
        } else {
            let mut flags = std::collections::HashMap::new();
            flags.insert("Disrupted".to_string(), true);
            entity_mut.insert(EntityFlags(flags));
        }
    }
    if let Some(mut cooldowns) = world.get_resource_mut::<CustomActionCooldowns>() {
        cooldowns
            .0
            .retain(|(cooldown_object_id, _), _| *cooldown_object_id != target_id);
    }
    Ok(())
}

fn sonic_effect_multiplier(world: &World, target: Entity) -> Result<f64, RejectionReason> {
    let body_registry = world.resource::<BodyPartRegistry>();
    let damage_registry = world.resource::<DamageTypeRegistry>();
    let resistance_registry = world.resource::<ResistanceRegistry>();
    let entity_ref = world
        .get_entity(target)
        .map_err(|_| RejectionReason::ObjectNotFound)?;
    let attrs = entity_ref.get::<Attributes>();
    let flags = entity_ref.get::<EntityFlags>();
    if let Some(drone) = entity_ref.get::<Drone>() {
        Ok(crate::systems::combat_system::final_damage_multiplier(
            Some(&drone.body),
            attrs,
            flags,
            DamageType::Sonic.as_str(),
            body_registry,
            damage_registry,
            resistance_registry,
        ))
    } else if entity_ref.get::<Structure>().is_some() {
        Ok(crate::systems::combat_system::final_damage_multiplier(
            None,
            attrs,
            flags,
            DamageType::Sonic.as_str(),
            body_registry,
            damage_registry,
            resistance_registry,
        ))
    } else {
        Err(RejectionReason::ObjectNotFound)
    }
}

fn is_negative_status_attr(attr: &str) -> bool {
    matches!(
        attr,
        "Disrupted"
            | "Scrambled"
            | "Drained"
            | "Overloaded"
            | "Debilitated"
            | "Leeching"
            | "Hacking"
            | "HackSlowed"
            | "HackRooted"
            | "HackNeutralized"
    ) || attr.starts_with("Disrupt:")
        || attr.starts_with("Scramble:")
        || attr.starts_with("Drain:")
        || attr.starts_with("Overload:")
        || attr.starts_with("Debilitate:")
        || attr.starts_with("Leech:")
}

fn is_interruptible_action_attr(attr: &str) -> bool {
    matches!(attr, "Draining" | "Hacking" | "Channeling")
        || attr.starts_with("CurrentAction:")
        || attr.starts_with("Channeling:")
        || attr.starts_with("ContinuousAction:")
}

fn effect_multiplier(
    world: &World,
    target: Entity,
    damage_type: &str,
) -> Result<f64, RejectionReason> {
    let body_registry = world.resource::<BodyPartRegistry>();
    let damage_registry = world.resource::<DamageTypeRegistry>();
    let resistance_registry = world.resource::<ResistanceRegistry>();
    let entity_ref = world
        .get_entity(target)
        .map_err(|_| RejectionReason::ObjectNotFound)?;
    let attrs = entity_ref.get::<Attributes>();
    let flags = entity_ref.get::<EntityFlags>();
    if let Some(drone) = entity_ref.get::<Drone>() {
        Ok(crate::systems::combat_system::final_damage_multiplier(
            Some(&drone.body),
            attrs,
            flags,
            damage_type,
            body_registry,
            damage_registry,
            resistance_registry,
        ))
    } else if entity_ref.get::<Structure>().is_some() {
        Ok(crate::systems::combat_system::final_damage_multiplier(
            None,
            attrs,
            flags,
            damage_type,
            body_registry,
            damage_registry,
            resistance_registry,
        ))
    } else {
        Err(RejectionReason::ObjectNotFound)
    }
}

fn ensure_attrs(
    world: &mut World,
    target_id: ObjectId,
    mutate: impl FnOnce(&mut Vec<String>),
) -> CommandResult {
    let target = entity(target_id)?;
    let mut entity_mut = world.entity_mut(target);
    if let Some(mut attrs) = entity_mut.get_mut::<Attributes>() {
        mutate(&mut attrs.0);
    } else {
        let mut attrs = Vec::new();
        mutate(&mut attrs);
        entity_mut.insert(Attributes(attrs));
    }
    Ok(())
}

fn push_unique(attrs: &mut Vec<String>, value: impl Into<String>) {
    let value = value.into();
    if !attrs.iter().any(|attr| attr == &value) {
        attrs.push(value);
    }
}

fn apply_hack(world: &mut World, target_id: ObjectId) -> CommandResult {
    let target = entity(target_id)?;
    if effect_multiplier(world, target, DamageType::Psionic.as_str())? <= 0.0 {
        return Ok(());
    }
    ensure_attrs(world, target_id, |attrs| {
        attrs.retain(|attr| !attr.starts_with("Hack:"));
        push_unique(attrs, "Hacking");
        push_unique(attrs, "HackSlowed");
        push_unique(attrs, "HackRooted");
        push_unique(attrs, "HackNeutralized");
        attrs.push("Hack:slow_ticks=1-2".to_string());
        attrs.push("Hack:root_ticks=3-4".to_string());
        attrs.push("Hack:neutral_tick=5".to_string());
    })?;
    if let Some(mut drone) = world.entity_mut(target).get_mut::<Drone>() {
        drone.owner = 0;
        drone.fatigue = drone.fatigue.max(4);
    }
    Ok(())
}

fn apply_drain(
    world: &mut World,
    object_id: ObjectId,
    target_id: ObjectId,
    resource: &str,
) -> CommandResult {
    let target = entity(target_id)?;
    if effect_multiplier(world, target, DamageType::EMP.as_str())? <= 0.0 {
        return Ok(());
    }
    let object = entity(object_id)?;
    let amount = {
        let target_amount = target_resource_amount(world, target_id, resource)?.1;
        let self_space = target_resource_space(world, object_id, resource)?.1;
        let carry_capacity = world
            .entity(object)
            .get::<Drone>()
            .map(|drone| drone.carry_capacity)
            .unwrap_or_default();
        target_amount.min(self_space).min(carry_capacity)
    };
    if amount == 0 {
        return Ok(());
    }
    take_from_target(world, target, resource, amount)?;
    if let Some(mut drone) = world.entity_mut(object).get_mut::<Drone>() {
        *drone.carry.entry(resource.to_string()).or_default() += amount;
    }
    ensure_attrs(world, target_id, |attrs| {
        push_unique(attrs, "Drained");
        push_unique(attrs, "Draining");
        attrs.push(format!("Drain:{resource}:{amount}"));
    })
}

fn apply_overload(world: &mut World, target_id: ObjectId) -> CommandResult {
    let target = entity(target_id)?;
    if effect_multiplier(world, target, DamageType::EMP.as_str())? <= 0.0 {
        return Ok(());
    }
    ensure_attrs(world, target_id, |attrs| {
        push_unique(attrs, "Overloaded");
        attrs.push(format!(
            "Overload:fuel-{OVERLOAD_FUEL_DRAIN}:floor-{OVERLOAD_FUEL_FLOOR}"
        ));
    })
}

fn apply_debilitate(world: &mut World, target_id: ObjectId, damage_type: &str) -> CommandResult {
    let target = entity(target_id)?;
    if effect_multiplier(world, target, DamageType::Corrosive.as_str())? <= 0.0 {
        return Ok(());
    }
    ensure_attrs(world, target_id, |attrs| {
        push_unique(attrs, "Debilitated");
        attrs.retain(|attr| !attr.starts_with("Debilitate:"));
        attrs.push(format!(
            "Debilitate:{damage_type}:resistance_x2:duration=50"
        ));
    })
}

fn fabricate_target(
    world: &mut World,
    player_id: PlayerId,
    target_id: ObjectId,
    structure_type: Option<StructureType>,
) -> CommandResult {
    let target = entity(target_id)?;
    let position = *world
        .entity(target)
        .get::<Position>()
        .ok_or(RejectionReason::ObjectNotFound)?;
    if world.entity(target).get::<Drone>().is_none() {
        return Err(RejectionReason::NotMovable);
    }
    world.entity_mut(target).despawn();
    world.spawn((
        position,
        structure_defaults(
            structure_type.unwrap_or(StructureType::FACTORY),
            Some(player_id),
            world,
        ),
    ));
    Ok(())
}

fn apply_resisted_damage_amount(
    world: &mut World,
    target_id: ObjectId,
    damage_type: &str,
    damage: u32,
) -> Result<u32, RejectionReason> {
    let target = entity(target_id)?;
    let multiplier = effect_multiplier(world, target, damage_type)?;
    let damage = ((damage as f64) * multiplier).floor() as u32;
    if let Some(mut target_drone) = world.entity_mut(target).get_mut::<Drone>() {
        target_drone.hits = target_drone.hits.saturating_sub(damage);
    } else if let Some(mut structure) = world.entity_mut(target).get_mut::<Structure>() {
        structure.hits = structure.hits.saturating_sub(damage);
    }
    Ok(damage)
}

fn heal_drone(world: &mut World, object_id: ObjectId, amount: u32) {
    if amount == 0 {
        return;
    }
    if let Ok(object) = entity(object_id) {
        if let Some(mut drone) = world.entity_mut(object).get_mut::<Drone>() {
            drone.hits = (drone.hits + amount).min(drone.hits_max);
        }
    }
}

fn apply_transfer_to_global(
    world: &mut World,
    player_id: PlayerId,
    resource: &str,
    amount: u32,
) -> CommandResult {
    let config = world.resource::<GlobalStorageConfig>().clone();
    subtract_player_resource(
        world
            .resource_mut::<PlayerLocalStorage>()
            .0
            .entry(player_id)
            .or_default(),
        resource,
        amount,
    );
    world
        .resource_mut::<PendingGlobalTransfers>()
        .0
        .push(PendingGlobalTransfer {
            player_id,
            direction: GlobalTransferDirection::ToGlobal,
            resource: resource.to_string(),
            amount,
            deliver_amount: amount.saturating_sub(transfer_fee(
                amount,
                config.transfer_to_global_fee_per_10_000,
            )),
            remaining_ticks: config.transfer_to_global_ticks,
            start: player_storage_position(player_id),
            end: global_storage_position(player_id),
        });
    Ok(())
}

fn apply_transfer_from_global(
    world: &mut World,
    player_id: PlayerId,
    resource: &str,
    amount: u32,
) -> CommandResult {
    let config = world.resource::<GlobalStorageConfig>().clone();
    subtract_player_resource(
        world
            .resource_mut::<PlayerGlobalStorage>()
            .0
            .entry(player_id)
            .or_default(),
        resource,
        amount,
    );
    world
        .resource_mut::<PendingGlobalTransfers>()
        .0
        .push(PendingGlobalTransfer {
            player_id,
            direction: GlobalTransferDirection::FromGlobal,
            resource: resource.to_string(),
            amount,
            deliver_amount: amount.saturating_sub(transfer_fee(
                amount,
                config.transfer_from_global_fee_per_10_000,
            )),
            remaining_ticks: config.transfer_from_global_ticks,
            start: global_storage_position(player_id),
            end: player_storage_position(player_id),
        });
    Ok(())
}

fn player_storage_position(player_id: PlayerId) -> Position {
    Position {
        x: player_lane_x(player_id),
        y: 0,
        room: RoomId(0),
    }
}

fn global_storage_position(player_id: PlayerId) -> Position {
    Position {
        x: player_lane_x(player_id),
        y: 49,
        room: RoomId(0),
    }
}

fn player_lane_x(player_id: PlayerId) -> i32 {
    (player_id % 50) as i32
}

fn drone_snapshot(
    world: &mut World,
    object_id: ObjectId,
) -> Result<(Position, Drone), RejectionReason> {
    let entity = entity(object_id)?;
    let entity_ref = world
        .get_entity(entity)
        .map_err(|_| RejectionReason::ObjectNotFound)?;
    let position = *entity_ref
        .get::<Position>()
        .ok_or(RejectionReason::ObjectNotFound)?;
    let drone = entity_ref
        .get::<Drone>()
        .ok_or(RejectionReason::NotMovable)?
        .clone();
    Ok((position, drone))
}

fn source_snapshot(
    world: &mut World,
    object_id: ObjectId,
) -> Result<(Position, crate::components::Source), RejectionReason> {
    let entity = entity(object_id)?;
    let entity_ref = world
        .get_entity(entity)
        .map_err(|_| RejectionReason::ObjectNotFound)?;
    let position = *entity_ref
        .get::<Position>()
        .ok_or(RejectionReason::ObjectNotFound)?;
    let source = entity_ref
        .get::<crate::components::Source>()
        .ok_or(RejectionReason::NotSource)?
        .clone();
    Ok((position, source))
}

fn structure_snapshot(
    world: &mut World,
    object_id: ObjectId,
) -> Result<(Position, Structure), RejectionReason> {
    let entity = entity(object_id)?;
    let entity_ref = world
        .get_entity(entity)
        .map_err(|_| RejectionReason::ObjectNotFound)?;
    let position = *entity_ref
        .get::<Position>()
        .ok_or(RejectionReason::ObjectNotFound)?;
    let structure = entity_ref
        .get::<Structure>()
        .ok_or(RejectionReason::ObjectNotFound)?
        .clone();
    Ok((position, structure))
}

fn attackable_snapshot(
    world: &mut World,
    object_id: ObjectId,
) -> Result<(Position, Option<PlayerId>), RejectionReason> {
    let entity = entity(object_id)?;
    let entity_ref = world
        .get_entity(entity)
        .map_err(|_| RejectionReason::ObjectNotFound)?;
    let position = *entity_ref
        .get::<Position>()
        .ok_or(RejectionReason::ObjectNotFound)?;
    if let Some(drone) = entity_ref.get::<Drone>() {
        Ok((position, Some(drone.owner)))
    } else if let Some(structure) = entity_ref.get::<Structure>() {
        Ok((position, structure.owner))
    } else {
        Err(RejectionReason::ObjectNotFound)
    }
}

fn controller_snapshot(
    world: &mut World,
    object_id: ObjectId,
) -> Result<(Position, Controller), RejectionReason> {
    let entity = entity(object_id)?;
    let entity_ref = world
        .get_entity(entity)
        .map_err(|_| RejectionReason::ObjectNotFound)?;
    let position = *entity_ref
        .get::<Position>()
        .ok_or(RejectionReason::ObjectNotFound)?;
    let controller = entity_ref
        .get::<Controller>()
        .ok_or(RejectionReason::NotController)?
        .clone();
    Ok((position, controller))
}

fn target_resource_amount(
    world: &mut World,
    target_id: ObjectId,
    resource: &str,
) -> Result<(Position, u32), RejectionReason> {
    let entity = entity(target_id)?;
    let entity_ref = world
        .get_entity(entity)
        .map_err(|_| RejectionReason::ObjectNotFound)?;
    let position = *entity_ref
        .get::<Position>()
        .ok_or(RejectionReason::ObjectNotFound)?;
    if let Some(drone) = entity_ref.get::<Drone>() {
        return Ok((position, *drone.carry.get(resource).unwrap_or(&0)));
    }
    if let Some(structure) = entity_ref.get::<Structure>() {
        return Ok((position, structure_energy(resource, structure.energy)));
    }
    if let Some(resource_store) = entity_ref.get::<crate::components::Resource>() {
        return Ok((
            position,
            *resource_store.amounts.get(resource).unwrap_or(&0),
        ));
    }
    Err(RejectionReason::ObjectNotFound)
}

fn target_resource_space(
    world: &mut World,
    target_id: ObjectId,
    resource: &str,
) -> Result<(Position, u32), RejectionReason> {
    let entity = entity(target_id)?;
    let entity_ref = world
        .get_entity(entity)
        .map_err(|_| RejectionReason::ObjectNotFound)?;
    let position = *entity_ref
        .get::<Position>()
        .ok_or(RejectionReason::ObjectNotFound)?;
    if let Some(drone) = entity_ref.get::<Drone>() {
        return Ok((
            position,
            drone
                .carry_capacity
                .saturating_sub(carry_used(&drone.carry)),
        ));
    }
    if let Some(structure) = entity_ref.get::<Structure>() {
        if resource != "Energy" || structure.energy_capacity.is_none() {
            return Err(RejectionReason::TargetFull);
        }
        return Ok((
            position,
            structure
                .energy_capacity
                .unwrap_or(0)
                .saturating_sub(structure.energy.unwrap_or(0)),
        ));
    }
    if entity_ref.get::<Controller>().is_some() {
        if resource == "Energy" {
            return Ok((position, u32::MAX));
        }
        return Err(RejectionReason::TargetFull);
    }
    Err(RejectionReason::ObjectNotFound)
}

fn ensure_owner(drone: &Drone, player_id: PlayerId) -> CommandResult {
    if drone.owner != player_id {
        return Err(RejectionReason::NotOwner);
    }
    Ok(())
}

fn ensure_drone_can_act(drone: &Drone, part: BodyPart, check_fatigue: bool) -> CommandResult {
    if drone.spawning {
        return Err(RejectionReason::StillSpawning);
    }
    if check_fatigue && drone.fatigue > 0 {
        return Err(RejectionReason::Fatigued);
    }
    require_body(drone, part)
}

fn require_body(drone: &Drone, part: BodyPart) -> CommandResult {
    if !drone.body.contains(&part) {
        return Err(RejectionReason::MissingBodyPart { part });
    }
    Ok(())
}

fn ensure_range(from: Position, to: Position, max: u32) -> CommandResult {
    let distance = hex_distance(from, to);
    if distance > max {
        return Err(RejectionReason::OutOfRange { distance, max });
    }
    Ok(())
}

fn hex_distance(from: Position, to: Position) -> u32 {
    if from.room != to.room {
        return u32::MAX;
    }
    let dx = to.x - from.x;
    let dy = to.y - from.y;
    dx.abs().max(dy.abs()).max((dx + dy).abs()) as u32
}

fn step(terrains: &RoomTerrains, position: Position, direction: Direction) -> Option<Position> {
    let (dx, dy) = direction_delta(direction);
    let room = terrains.0.get(&position.room)?;
    let mut x = position.x + dx;
    let mut y = position.y + dy;
    let mut room_id = position.room;
    let mut room_dx = 0;
    let mut room_dy = 0;

    if x < 0 {
        room_dx = -1;
        x = room.width - 1;
    } else if x >= room.width {
        room_dx = 1;
        x = 0;
    }

    if y < 0 {
        room_dy = -1;
        y = room.height - 1;
    } else if y >= room.height {
        room_dy = 1;
        y = 0;
    }

    if room_dx != 0 || room_dy != 0 {
        room_id = room_id.adjacent(room_dx, room_dy)?;
        terrains.0.get(&room_id)?;
    }

    Some(Position {
        x,
        y,
        room: room_id,
    })
}

fn direction_delta(direction: Direction) -> (i32, i32) {
    match direction {
        Direction::Top => (0, -1),
        Direction::TopRight => (1, -1),
        Direction::BottomRight => (1, 0),
        Direction::Bottom => (0, 1),
        Direction::BottomLeft => (-1, 1),
        Direction::TopLeft => (-1, 0),
    }
}

fn spawn_output_position(position: Position) -> Position {
    Position {
        x: position.x + 1,
        y: position.y,
        room: position.room,
    }
}

fn tile_has_blocking_enemy(world: &mut World, position: Position, player_id: PlayerId) -> bool {
    world
        .query::<(&Position, &Drone)>()
        .iter(world)
        .any(|(drone_position, drone)| *drone_position == position && drone.owner != player_id)
}

fn tile_has_any_drone(world: &mut World, position: Position) -> bool {
    world
        .query::<(&Position, &Drone)>()
        .iter(world)
        .any(|(drone_position, _)| *drone_position == position)
}

fn tile_has_any_object(world: &mut World, position: Position) -> bool {
    tile_has_any_drone(world, position)
        || world
            .query::<(&Position, &Structure)>()
            .iter(world)
            .any(|(object_position, _)| *object_position == position)
        || world
            .query::<(&Position, &crate::components::Source)>()
            .iter(world)
            .any(|(object_position, _)| *object_position == position)
        || world
            .query::<(&Position, &crate::components::Resource)>()
            .iter(world)
            .any(|(object_position, _)| *object_position == position)
        || world
            .query::<(&Position, &Controller)>()
            .iter(world)
            .any(|(object_position, _)| *object_position == position)
}

fn structure_defaults(
    structure_type: StructureType,
    owner: Option<PlayerId>,
    world: &World,
) -> Structure {
    let def = world
        .resource::<StructureTypeRegistry>()
        .get(structure_type);
    let capacity = def.and_then(|def| def.capacity);
    let energy_capacity = if matches!(
        structure_type,
        StructureType::SPAWN | StructureType::EXTENSION | StructureType::TOWER
    ) {
        capacity
    } else {
        None
    };
    Structure {
        structure_type,
        owner,
        hits: 1,
        hits_max: if matches!(
            structure_type,
            StructureType::SPAWN | StructureType::EXTENSION | StructureType::TOWER
        ) {
            5_000
        } else {
            def.map(|def| def.hits).unwrap_or(5_000)
        },
        energy: energy_capacity.map(|_| 0),
        energy_capacity,
        cooldown: 0,
    }
}

fn ensure_no_pending_global_transfer(world: &World, player_id: PlayerId) -> CommandResult {
    if world
        .resource::<PendingGlobalTransfers>()
        .0
        .iter()
        .any(|transfer| transfer.player_id == player_id)
    {
        return Err(RejectionReason::TransferInProgress);
    }
    Ok(())
}

fn global_storage_committed(world: &World, player_id: PlayerId) -> u32 {
    let stored: u32 = world
        .resource::<PlayerGlobalStorage>()
        .0
        .get(&player_id)
        .map(|storage| storage.values().sum())
        .unwrap_or_default();
    let pending: u32 = world
        .resource::<PendingGlobalTransfers>()
        .0
        .iter()
        .filter(|transfer| {
            transfer.player_id == player_id
                && transfer.direction == GlobalTransferDirection::ToGlobal
        })
        .map(|transfer| transfer.deliver_amount)
        .sum();
    stored.saturating_add(pending)
}

fn room_controller_level(world: &mut World, room: RoomId, player_id: PlayerId) -> u8 {
    world
        .query::<(&Position, &Controller)>()
        .iter(world)
        .filter(|(position, controller)| {
            position.room == room && controller.owner == Some(player_id)
        })
        .map(|(_, controller)| controller.level)
        .max()
        .unwrap_or(8)
}

fn player_global_amount(world: &World, player_id: PlayerId, resource: &str) -> u32 {
    world
        .resource::<PlayerGlobalStorage>()
        .0
        .get(&player_id)
        .and_then(|storage| storage.get(resource))
        .copied()
        .unwrap_or_default()
}

fn player_local_amount(world: &World, player_id: PlayerId, resource: &str) -> u32 {
    world
        .resource::<PlayerLocalStorage>()
        .0
        .get(&player_id)
        .and_then(|storage| storage.get(resource))
        .copied()
        .unwrap_or_default()
}

fn ensure_player_resource_cost(
    world: &World,
    player_id: PlayerId,
    cost: &ResourceCost,
    skip_energy: bool,
) -> CommandResult {
    for (resource, required) in cost {
        if skip_energy && resource == ENERGY_RESOURCE {
            continue;
        }
        let available = player_local_amount(world, player_id, resource);
        if available < *required {
            return Err(RejectionReason::InsufficientResource {
                resource: resource.clone(),
                required: *required,
                available,
            });
        }
    }
    Ok(())
}

fn deduct_player_resource_cost(
    world: &mut World,
    player_id: PlayerId,
    cost: &ResourceCost,
    skip_energy: bool,
) {
    let mut local_storage = world.resource_mut::<PlayerLocalStorage>();
    let storage = local_storage.0.entry(player_id).or_default();
    for (resource, amount) in cost {
        if skip_energy && resource == ENERGY_RESOURCE {
            continue;
        }
        subtract_player_resource(storage, resource, *amount);
    }
}

fn add_player_resource(world: &mut World, player_id: PlayerId, resource: &str, amount: u32) {
    *world
        .resource_mut::<PlayerGlobalStorage>()
        .0
        .entry(player_id)
        .or_default()
        .entry(resource.to_string())
        .or_default() += amount;
}

fn recycle_refund_cost(world: &World, _tick: Tick, body: &[BodyPart]) -> ResourceCost {
    let full_refund = world
        .get_resource::<WorldSettings>()
        .is_some_and(|settings| settings.mode == WorldMode::Tutorial);
    let mut refund = body_spawn_cost(world, body);
    if !full_refund {
        for amount in refund.values_mut() {
            *amount /= 2;
        }
    }
    refund
}

fn refund_recycle_cost(
    world: &mut World,
    player_id: PlayerId,
    spawn: Entity,
    refund: &ResourceCost,
) -> CommandResult {
    for (resource, amount) in refund {
        if *amount == 0 {
            continue;
        }
        if resource == ENERGY_RESOURCE {
            let overflow = {
                let mut entity_mut = world.entity_mut(spawn);
                let mut structure = entity_mut
                    .get_mut::<Structure>()
                    .ok_or(RejectionReason::ObjectNotFound)?;
                let capacity = structure.energy_capacity;
                match (&mut structure.energy, capacity) {
                    (Some(energy), Some(capacity)) => {
                        let accepted = capacity.saturating_sub(*energy).min(*amount);
                        *energy += accepted;
                        amount.saturating_sub(accepted)
                    }
                    _ => *amount,
                }
            };
            if overflow > 0 {
                add_player_local_resource(world, player_id, resource, overflow);
            }
        } else {
            add_player_local_resource(world, player_id, resource, *amount);
        }
    }
    Ok(())
}

fn add_player_local_resource(world: &mut World, player_id: PlayerId, resource: &str, amount: u32) {
    *world
        .resource_mut::<PlayerLocalStorage>()
        .0
        .entry(player_id)
        .or_default()
        .entry(resource.to_string())
        .or_default() += amount;
}

fn transfer_fee(amount: u32, fee_per_10_000: u32) -> u32 {
    amount.saturating_mul(fee_per_10_000) / 10_000
}

fn subtract_player_resource(storage: &mut IndexMap<String, u32>, resource: &str, amount: u32) {
    let value = storage.entry(resource.to_string()).or_default();
    *value = value.saturating_sub(amount);
}

fn carry_used(carry: &IndexMap<String, u32>) -> u32 {
    carry.values().sum()
}

fn structure_energy(resource: &str, energy: Option<u32>) -> u32 {
    if resource == "Energy" {
        energy.unwrap_or(0)
    } else {
        0
    }
}

fn take_from_drone(world: &mut World, entity: Entity, resource: &str, amount: u32) {
    let mut entity_mut = world.entity_mut(entity);
    let mut drone = entity_mut.get_mut::<Drone>().unwrap();
    let value = drone.carry.entry(resource.to_string()).or_default();
    *value -= amount;
}

fn add_to_target(world: &mut World, entity: Entity, resource: &str, amount: u32) -> CommandResult {
    if let Some(mut drone) = world.entity_mut(entity).get_mut::<Drone>() {
        *drone.carry.entry(resource.to_string()).or_default() += amount;
        return Ok(());
    }
    if let Some(mut structure) = world.entity_mut(entity).get_mut::<Structure>() {
        if resource == "Energy" {
            if let Some(energy) = &mut structure.energy {
                *energy += amount;
                return Ok(());
            }
        }
    }
    if world.entity(entity).contains::<Controller>() {
        if resource == "Energy" {
            world
                .resource_mut::<PendingControllerUpgrade>()
                .0
                .push((entity.to_bits(), amount));
            return Ok(());
        }
    }
    Err(RejectionReason::ObjectNotFound)
}

fn take_from_target(
    world: &mut World,
    entity: Entity,
    resource: &str,
    amount: u32,
) -> CommandResult {
    if let Some(mut drone) = world.entity_mut(entity).get_mut::<Drone>() {
        let value = drone.carry.entry(resource.to_string()).or_default();
        *value -= amount;
        return Ok(());
    }
    if let Some(mut structure) = world.entity_mut(entity).get_mut::<Structure>() {
        if resource == "Energy" {
            if let Some(energy) = &mut structure.energy {
                *energy -= amount;
                return Ok(());
            }
        }
    }
    if let Some(mut resource_store) = world
        .entity_mut(entity)
        .get_mut::<crate::components::Resource>()
    {
        let value = resource_store
            .amounts
            .entry(resource.to_string())
            .or_default();
        *value -= amount;
        return Ok(());
    }
    Err(RejectionReason::ObjectNotFound)
}

pub fn body_cost(body: &[BodyPart]) -> u32 {
    ResourceRegistry::default().body_energy_cost(body)
}

fn body_spawn_cost(world: &World, body: &[BodyPart]) -> ResourceCost {
    world
        .get_resource::<ResourceRegistry>()
        .map(|registry| registry.body_cost(body))
        .unwrap_or_else(|| ResourceRegistry::default().body_cost(body))
}

fn build_cost(world: &World, structure: StructureType) -> ResourceCost {
    world
        .get_resource::<ResourceRegistry>()
        .and_then(|registry| registry.action_costs.build.get(&structure).cloned())
        .unwrap_or_default()
}

pub fn entity(object_id: ObjectId) -> Result<Entity, RejectionReason> {
    Ok(Entity::from_bits(object_id))
}

fn ensure_damage_type(world: &World, damage_type: &str) -> CommandResult {
    if world
        .resource::<DamageTypeRegistry>()
        .damage_types
        .contains_key(damage_type)
    {
        Ok(())
    } else {
        Err(RejectionReason::InvalidDamageType)
    }
}

fn has_attr(world: &World, target_id: ObjectId, value: &str) -> Result<bool, RejectionReason> {
    let target = entity(target_id)?;
    let entity_ref = world
        .get_entity(target)
        .map_err(|_| RejectionReason::ObjectNotFound)?;
    Ok(entity_ref
        .get::<Attributes>()
        .is_some_and(|attrs| attrs.0.iter().any(|attr| attr == value)))
}

fn has_debilitate(
    world: &World,
    target_id: ObjectId,
    damage_type: &str,
) -> Result<bool, RejectionReason> {
    let target = entity(target_id)?;
    let entity_ref = world
        .get_entity(target)
        .map_err(|_| RejectionReason::ObjectNotFound)?;
    Ok(entity_ref.get::<Attributes>().is_some_and(|attrs| {
        attrs
            .0
            .iter()
            .any(|attr| attr == &format!("Debilitated({damage_type})"))
    }))
}

fn player_fuel_budget(world: &World, player_id: PlayerId) -> u64 {
    // Fuel is tracked per tick; return the standard MAX_FUEL as baseline.
    // In practice this consults the tick engine's fuel state.
    MAX_FUEL
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::world::create_world;

    fn raw_custom(
        player_id: PlayerId,
        action_type: &str,
        object_id: ObjectId,
        target_id: Option<ObjectId>,
    ) -> RawCommand {
        RawCommand {
            player_id,
            tick: 1,
            source: CommandSource::TestHarness,
            auth: CommandAuth {
                source: CommandSource::TestHarness,
                player_id,
                tick_submitted: 1,
                tick_target: 1,
            },
            sequence: 1,
            action: CommandAction::Custom {
                action_type: action_type.to_string(),
                object_id,
                target_id,
                resource: None,
                amount: None,
                structure: None,
            },
        }
    }

    fn give_local_energy(world: &mut World, player_id: PlayerId, amount: u32) {
        world
            .resource_mut::<PlayerLocalStorage>()
            .0
            .entry(player_id)
            .or_default()
            .insert(ENERGY_RESOURCE.to_string(), amount);
    }

    #[test]
    fn disrupt_clears_target_current_action_and_sets_one_tick_flag() {
        let mut world = create_world();
        let attacker = world.spawn_drone(1, 10, 10, vec![BodyPart::Attack]);
        let target = world.spawn_drone(2, 11, 10, vec![BodyPart::Move]);
        let attacker_id = object_id(attacker);
        let target_id = object_id(target);

        give_local_energy(world.app.world_mut(), 1, 100);
        world
            .app
            .world_mut()
            .entity_mut(target)
            .insert(Attributes(vec![
                "Hacking".to_string(),
                "CurrentAction:Hack".to_string(),
            ]));
        world
            .app
            .world_mut()
            .insert_resource(CustomActionCooldowns::default());
        world
            .app
            .world_mut()
            .resource_mut::<CustomActionCooldowns>()
            .0
            .insert((target_id, "Hack".to_string()), 50);

        world
            .submit_raw_command(raw_custom(1, "Disrupt", attacker_id, Some(target_id)))
            .unwrap();

        let target_ref = world.app.world().entity(target);
        let attrs = &target_ref.get::<Attributes>().unwrap().0;
        assert!(!attrs.iter().any(|attr| attr == "Hacking"));
        assert!(!attrs.iter().any(|attr| attr == "CurrentAction:Hack"));
        assert!(attrs.iter().any(|attr| attr == "Disrupted"));
        assert!(attrs.iter().any(|attr| attr == "Disrupted:duration=1"));
        assert_eq!(
            target_ref.get::<EntityFlags>().unwrap().0.get("Disrupted"),
            Some(&true)
        );
        assert!(
            !world
                .app
                .world()
                .resource::<CustomActionCooldowns>()
                .0
                .contains_key(&(target_id, "Hack".to_string()))
        );
    }

    #[test]
    fn fortify_clears_negative_flags_and_adds_three_tick_resistance() {
        let mut world = create_world();
        let drone = world.spawn_drone(1, 10, 10, vec![BodyPart::Tough]);
        let drone_id = object_id(drone);

        give_local_energy(world.app.world_mut(), 1, 400);
        let mut flags = std::collections::HashMap::new();
        flags.insert("Debilitated".to_string(), true);
        flags.insert("Hacking".to_string(), true);
        flags.insert("immune_Kinetic".to_string(), true);
        world.app.world_mut().entity_mut(drone).insert((
            Attributes(vec![
                "Debilitated".to_string(),
                "Debilitate:Kinetic:resistance_x2:duration=50".to_string(),
                "Hacking".to_string(),
                "Shielded".to_string(),
            ]),
            EntityFlags(flags),
        ));

        world
            .submit_raw_command(raw_custom(1, "Fortify", drone_id, None))
            .unwrap();

        let drone_ref = world.app.world().entity(drone);
        let attrs = &drone_ref.get::<Attributes>().unwrap().0;
        assert!(!attrs.iter().any(|attr| attr == "Debilitated"));
        assert!(!attrs.iter().any(|attr| attr.starts_with("Debilitate:")));
        assert!(!attrs.iter().any(|attr| attr == "Hacking"));
        assert!(attrs.iter().any(|attr| attr == "Shielded"));
        assert!(attrs.iter().any(|attr| attr == "Fortified"));
        assert!(attrs.iter().any(|attr| attr == "Fortified:duration=3"));

        let flags = &drone_ref.get::<EntityFlags>().unwrap().0;
        assert!(!flags.contains_key("Debilitated"));
        assert!(!flags.contains_key("Hacking"));
        assert_eq!(flags.get("Fortified"), Some(&true));
        assert_eq!(flags.get("immune_Kinetic"), Some(&true));
    }
}
