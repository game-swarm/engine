use bevy::prelude::*;
use indexmap::IndexMap;
use serde::{Deserialize, Serialize};
use std::collections::HashSet;

use crate::components::*;
use crate::onboarding::{OnboardingEvent, send_onboarding_event};
use crate::resources::{
    GlobalStorageConfig, GlobalTransferDirection, MarketConfig, MarketOrder, MarketOrders,
    PendingGlobalTransfer, PendingGlobalTransfers, PlayerGlobalStorage, PlayerLocalStorage,
    ResourceCost, ResourceRegistry,
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

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
#[serde(tag = "type")]
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
    #[serde(rename = "Spawn")]
    SpawnDrone {
        spawn_id: ObjectId,
        body: Vec<BodyPart>,
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
    CreateMarketOrder {
        resource: String,
        amount: u32,
        price_resource: String,
        price_amount: u32,
    },
    BuyMarketOrder {
        order_id: u64,
    },
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
    SourceNotAllowed,
    AuthContextInvalid,
    ObjectNotFound,
    NotOwner,
    NotMovable,
    Fatigued,
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
    InvalidTerrain,
    TooManyConstructionSites,
    NotStructure,
    NotController,
    AlreadyFullHealth,
    FriendlyTarget,
    NotYourSpawn,
    SpawnOnCooldown,
    BodyTooLarge,
    ExceedsRoomCapacity,
    RoomDroneCapReached,
    NotFriendly,
    GlobalStorageDisabled,
    TransferInProgress,
    TerminalRequired,
    OrderNotFound,
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
        CommandAction::SpawnDrone { spawn_id, body } => {
            validate_spawn_drone(world, raw.player_id, *spawn_id, body)
        }
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
        CommandAction::CreateMarketOrder {
            resource,
            amount,
            price_resource,
            price_amount,
        } => validate_create_market_order(
            world,
            raw.player_id,
            resource,
            *amount,
            price_resource,
            *price_amount,
        ),
        CommandAction::BuyMarketOrder { order_id } => {
            validate_buy_market_order(world, raw.player_id, *order_id)
        }
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
            trigger_combat: false,
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
            trigger_combat: true,
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
        CommandSource::Tutorial => {
            !action_triggers_combat(action) && !action_uses_global_storage(action)
        }
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
        CommandAction::TransferToGlobal { .. }
            | CommandAction::TransferFromGlobal { .. }
            | CommandAction::CreateMarketOrder { .. }
            | CommandAction::BuyMarketOrder { .. }
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
        CommandAction::SpawnDrone { .. } => "Spawn",
        CommandAction::Build { .. } => "Build",
        CommandAction::TransferToGlobal { .. } => "TransferToGlobal",
        CommandAction::TransferFromGlobal { .. } => "TransferFromGlobal",
        CommandAction::CreateMarketOrder { .. } => "CreateMarketOrder",
        CommandAction::BuyMarketOrder { .. } => "BuyMarketOrder",
    };

    match rejection {
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
            CommandAction::SpawnDrone { spawn_id, body } => serde_json::json!({
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
    }
}

fn default_rejection_detail(
    command: &RawCommand,
    rejection: &RejectionReason,
    action: &str,
) -> serde_json::Value {
    serde_json::json!({
        "reason": rejection,
        "action": action,
        "player_id": command.player_id,
        "sequence": command.sequence,
        "source": command.source,
    })
}

fn json_depth(value: &serde_json::Value) -> usize {
    match value {
        serde_json::Value::Array(items) => 1 + items.iter().map(json_depth).max().unwrap_or(0),
        serde_json::Value::Object(fields) => 1 + fields.values().map(json_depth).max().unwrap_or(0),
        _ => 1,
    }
}

pub fn apply_command(world: &mut World, command: ValidatedCommand) -> CommandResult {
    match command.raw.action {
        CommandAction::Move {
            object_id,
            direction,
        } => apply_move(world, object_id, direction),
        CommandAction::Harvest {
            object_id,
            target_id,
            resource,
        } => apply_harvest(world, object_id, target_id, resource),
        CommandAction::Transfer {
            object_id,
            target_id,
            resource,
            amount,
        } => apply_transfer(world, object_id, target_id, &resource, amount),
        CommandAction::Withdraw {
            object_id,
            target_id,
            resource,
            amount,
        } => apply_withdraw(world, object_id, target_id, &resource, amount),
        CommandAction::Attack {
            object_id,
            target_id,
        } => apply_attack(world, object_id, target_id),
        CommandAction::RangedAttack {
            object_id,
            target_id,
            range: _,
        } => apply_ranged_attack(world, object_id, target_id),
        CommandAction::Heal {
            object_id,
            target_id,
        } => apply_heal(world, object_id, target_id),
        CommandAction::ClaimController {
            object_id: _,
            controller_id,
        } => apply_claim_controller(world, command.raw.player_id, controller_id),
        CommandAction::SpawnDrone { spawn_id, body } => {
            apply_spawn_drone(world, command.raw.player_id, spawn_id, body)
        }
        CommandAction::Build {
            object_id,
            x,
            y,
            structure,
        } => apply_build(world, command.raw.player_id, object_id, x, y, structure),
        CommandAction::TransferToGlobal { resource, amount } => {
            apply_transfer_to_global(world, command.raw.player_id, &resource, amount)
        }
        CommandAction::TransferFromGlobal { resource, amount } => {
            apply_transfer_from_global(world, command.raw.player_id, &resource, amount)
        }
        CommandAction::CreateMarketOrder {
            resource,
            amount,
            price_resource,
            price_amount,
        } => apply_create_market_order(
            world,
            command.raw.player_id,
            &resource,
            amount,
            &price_resource,
            price_amount,
        ),
        CommandAction::BuyMarketOrder { order_id } => {
            apply_buy_market_order(world, command.raw.player_id, order_id)
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
    if structure.structure_type != StructureType::Spawn || structure.owner != Some(player_id) {
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
    let cost = build_cost(world, structure);
    ensure_player_resource_cost(world, player_id, &cost, false)?;

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
    ensure_range(position, target, 1)
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

fn validate_create_market_order(
    world: &mut World,
    player_id: PlayerId,
    resource: &str,
    amount: u32,
    _price_resource: &str,
    _price_amount: u32,
) -> CommandResult {
    ensure_market_enabled(world, player_id)?;
    let available = player_global_amount(world, player_id, resource);
    if available < amount {
        return Err(RejectionReason::InsufficientResource {
            resource: resource.to_string(),
            required: amount,
            available,
        });
    }
    Ok(())
}

fn validate_buy_market_order(
    world: &mut World,
    player_id: PlayerId,
    order_id: u64,
) -> CommandResult {
    ensure_market_enabled(world, player_id)?;
    let order = world
        .resource::<MarketOrders>()
        .orders
        .get(&order_id)
        .cloned()
        .ok_or(RejectionReason::OrderNotFound)?;

    let available = player_global_amount(world, player_id, &order.price_resource);
    if available < order.price_amount {
        return Err(RejectionReason::InsufficientResource {
            resource: order.price_resource,
            required: order.price_amount,
            available,
        });
    }

    let config = world.resource::<GlobalStorageConfig>();
    if global_storage_committed(world, player_id).saturating_add(order.amount) > config.capacity {
        return Err(RejectionReason::TargetFull);
    }
    if global_storage_committed(world, order.seller).saturating_add(order.price_amount)
        > config.capacity
    {
        return Err(RejectionReason::TargetFull);
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
        let entity_ref = world
            .get_entity(target)
            .map_err(|_| RejectionReason::ObjectNotFound)?;
        let attrs = entity_ref.get::<Attributes>();
        if let Some(drone) = entity_ref.get::<Drone>() {
            let body_mult = drone.body.iter().fold(1.0, |m, p| {
                m * (1.0 - body_registry.resistance(*p, damage_type))
            });
            body_mult * damage_registry.attribute_multiplier(damage_type, attrs)
        } else if entity_ref.get::<Structure>().is_some() {
            damage_registry.attribute_multiplier(damage_type, attrs)
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
        structure_defaults(structure_type, Some(player_id)),
    ));
    send_onboarding_event(world, OnboardingEvent::StructureBuilt);
    Ok(())
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

fn apply_create_market_order(
    world: &mut World,
    player_id: PlayerId,
    resource: &str,
    amount: u32,
    price_resource: &str,
    price_amount: u32,
) -> CommandResult {
    subtract_player_resource(
        world
            .resource_mut::<PlayerGlobalStorage>()
            .0
            .entry(player_id)
            .or_default(),
        resource,
        amount,
    );

    let mut orders = world.resource_mut::<MarketOrders>();
    let id = orders.next_order_id;
    orders.next_order_id += 1;
    orders.orders.insert(
        id,
        MarketOrder {
            id,
            seller: player_id,
            resource: resource.to_string(),
            amount,
            price_resource: price_resource.to_string(),
            price_amount,
        },
    );
    Ok(())
}

fn apply_buy_market_order(world: &mut World, player_id: PlayerId, order_id: u64) -> CommandResult {
    let order = world
        .resource_mut::<MarketOrders>()
        .orders
        .shift_remove(&order_id)
        .ok_or(RejectionReason::OrderNotFound)?;

    subtract_player_resource(
        world
            .resource_mut::<PlayerGlobalStorage>()
            .0
            .entry(player_id)
            .or_default(),
        &order.price_resource,
        order.price_amount,
    );
    add_player_resource(world, player_id, &order.resource, order.amount);
    add_player_resource(
        world,
        order.seller,
        &order.price_resource,
        order.price_amount,
    );
    Ok(())
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

fn structure_defaults(structure_type: StructureType, owner: Option<PlayerId>) -> Structure {
    let (energy, energy_capacity) = match structure_type {
        StructureType::Spawn => (Some(0), Some(300)),
        StructureType::Extension => (Some(0), Some(50)),
        StructureType::Tower => (Some(0), Some(1_000)),
        _ => (None, None),
    };
    Structure {
        structure_type,
        owner,
        hits: 1,
        hits_max: 5_000,
        energy,
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

fn ensure_market_enabled(world: &mut World, player_id: PlayerId) -> CommandResult {
    if !world.resource::<GlobalStorageConfig>().enabled {
        return Err(RejectionReason::GlobalStorageDisabled);
    }
    if world.resource::<MarketConfig>().market_requires_terminal && !owns_terminal(world, player_id)
    {
        return Err(RejectionReason::TerminalRequired);
    }
    Ok(())
}

fn owns_terminal(world: &mut World, player_id: PlayerId) -> bool {
    world.query::<&Structure>().iter(world).any(|structure| {
        structure.owner == Some(player_id)
            && matches!(structure.structure_type, StructureType::Terminal)
    })
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

fn entity(object_id: ObjectId) -> Result<Entity, RejectionReason> {
    Ok(Entity::from_bits(object_id))
}
