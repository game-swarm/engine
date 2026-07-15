use bevy::prelude::*;
use indexmap::IndexMap;
use serde::de::DeserializeOwned;
use serde::ser::SerializeMap;
use serde::{Deserialize, Deserializer, Serialize, Serializer};
use std::collections::{BTreeSet, HashSet};

use crate::components::*;
use crate::onboarding::{OnboardingEvent, send_onboarding_event};
use crate::resource_ledger::{ResourceLedger, ResourceOperation};
use crate::resources::{
    ALLIED_DAILY_CAP, ALLIED_TRANSFER_COOLDOWN, ALLIED_TRANSFER_DELAY, ALLIED_TRANSFER_FEE_BP,
    AlliedTransferCooldowns, AlliedTransferDailyUsage, CurrentTick, GlobalStorageConfig,
    GlobalTransferDirection, PendingAlliedTransfer, PendingAlliedTransfers, PendingGlobalTransfer,
    PendingGlobalTransfers, PlayerGlobalStorage, PlayerLocalStorage, ResourceCost,
    ResourceRegistry,
};
use crate::systems::{
    PendingControllerUpgrade, PendingDamage, PendingHeal, PendingSpawn, PendingSpawnQueue,
    PendingSpecialAttack, RoomDroneCounts, SpecialAttackKind, StatusActionIntent,
};

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
const OVERLOAD_FUEL_FLOOR: u64 = MAX_FUEL / 5;
const ADMIN_SCOPE: &str = "swarm:admin";

#[derive(Resource, Debug, Clone, Default)]
pub struct CustomActionCooldowns(pub(crate) IndexMap<(ObjectId, String), Tick>);

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
    Action {
        action_type: String,
        object_id: ObjectId,
        target_id: Option<ObjectId>,
        payload: serde_json::Value,
    },
    ClaimController {
        object_id: ObjectId,
        target_id: ObjectId,
    },
    Spawn {
        object_id: ObjectId,
        spawn_id: ObjectId,
        body_parts: Vec<BodyPart>,
    },
    Recycle {
        object_id: ObjectId,
    },
    Build {
        object_id: ObjectId,
        x: i32,
        y: i32,
        structure: StructureType,
    },
    Repair {
        object_id: ObjectId,
        target_id: ObjectId,
    },
    UpgradeController {
        object_id: ObjectId,
        target_id: ObjectId,
    },
    TransferToGlobal {
        resource: String,
        amount: u32,
    },
    TransferFromGlobal {
        resource: String,
        amount: u32,
    },
    AlliedTransfer {
        target_player: PlayerId,
        resource: String,
        amount: u32,
    },
}

pub const CORE_COMMAND_ACTIONS: &[&str] = &[
    "Move",
    "Harvest",
    "Transfer",
    "Withdraw",
    "Action",
    "ClaimController",
    "Spawn",
    "Recycle",
    "Build",
    "Repair",
    "UpgradeController",
    "TransferToGlobal",
    "TransferFromGlobal",
    "AlliedTransfer",
];

pub const SPECIAL_COMMAND_ACTIONS: &[&str] = &[
    "Attack",
    "RangedAttack",
    "Heal",
    "Hack",
    "Drain",
    "Overload",
    "Debilitate",
    "Disrupt",
    "Fortify",
    "Leech",
    "Fabricate",
];

impl<'de> Deserialize<'de> for CommandAction {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let value = serde_json::Value::deserialize(deserializer)?;
        let fields = value
            .as_object()
            .ok_or_else(|| serde::de::Error::custom("command action must be an object"))?;
        let command_type: String = required_action_field(fields, "type")?;

        Ok(match command_type.as_str() {
            "Move" => Self::Move {
                object_id: required_exact_action_field(fields, MOVE_ACTION_FIELDS, "object_id")?,
                direction: required_action_field(fields, "direction")?,
            },
            "Harvest" => Self::Harvest {
                object_id: required_exact_action_field(fields, HARVEST_ACTION_FIELDS, "object_id")?,
                target_id: required_action_field(fields, "target_id")?,
                resource: optional_action_field(fields, "resource")?,
            },
            "Transfer" => Self::Transfer {
                object_id: required_exact_action_field(
                    fields,
                    TRANSFER_ACTION_FIELDS,
                    "object_id",
                )?,
                target_id: required_action_field(fields, "target_id")?,
                resource: required_action_field(fields, "resource")?,
                amount: required_action_field(fields, "amount")?,
            },
            "Withdraw" => Self::Withdraw {
                object_id: required_exact_action_field(
                    fields,
                    TRANSFER_ACTION_FIELDS,
                    "object_id",
                )?,
                target_id: required_action_field(fields, "target_id")?,
                resource: required_action_field(fields, "resource")?,
                amount: required_action_field(fields, "amount")?,
            },
            "Action" => {
                return Err(serde::de::Error::custom(
                    "wire type Action is internal; use the concrete registered action name",
                ));
            }
            "ClaimController" => Self::ClaimController {
                object_id: required_exact_action_field(
                    fields,
                    CLAIM_CONTROLLER_ACTION_FIELDS,
                    "object_id",
                )?,
                target_id: required_action_field(fields, "target_id")?,
            },
            "Spawn" => {
                let body_parts: Vec<BodyPart> = required_action_field(fields, "body_parts")?;
                if body_parts.is_empty() {
                    return Err(serde::de::Error::custom(
                        "body_parts must contain at least one body part",
                    ));
                }
                Self::Spawn {
                    object_id: required_exact_action_field(
                        fields,
                        SPAWN_ACTION_FIELDS,
                        "object_id",
                    )?,
                    spawn_id: required_action_field(fields, "spawn_id")?,
                    body_parts,
                }
            }
            "Recycle" => Self::Recycle {
                object_id: required_exact_action_field(fields, RECYCLE_ACTION_FIELDS, "object_id")?,
            },
            "Build" => Self::Build {
                object_id: required_exact_action_field(fields, BUILD_ACTION_FIELDS, "object_id")?,
                x: required_action_field(fields, "x")?,
                y: required_action_field(fields, "y")?,
                structure: required_action_field(fields, "structure")?,
            },
            "Repair" => Self::Repair {
                object_id: required_exact_action_field(fields, TARGET_ACTION_FIELDS, "object_id")?,
                target_id: required_action_field(fields, "target_id")?,
            },
            "UpgradeController" => Self::UpgradeController {
                object_id: required_exact_action_field(fields, TARGET_ACTION_FIELDS, "object_id")?,
                target_id: required_action_field(fields, "target_id")?,
            },
            "TransferToGlobal" => Self::TransferToGlobal {
                resource: required_exact_action_field(
                    fields,
                    GLOBAL_TRANSFER_ACTION_FIELDS,
                    "resource",
                )?,
                amount: required_action_field(fields, "amount")?,
            },
            "TransferFromGlobal" => Self::TransferFromGlobal {
                resource: required_exact_action_field(
                    fields,
                    GLOBAL_TRANSFER_ACTION_FIELDS,
                    "resource",
                )?,
                amount: required_action_field(fields, "amount")?,
            },
            "AlliedTransfer" => Self::AlliedTransfer {
                target_player: required_exact_action_field(
                    fields,
                    ALLIED_TRANSFER_ACTION_FIELDS,
                    "target_player",
                )?,
                resource: required_action_field(fields, "resource")?,
                amount: required_action_field(fields, "amount")?,
            },
            action if SPECIAL_COMMAND_ACTIONS.contains(&action) => Self::Action {
                action_type: action.to_string(),
                object_id: required_action_field(fields, "object_id")?,
                target_id: Some(required_action_field(fields, "target_id")?),
                payload: command_action_payload(fields)?,
            },
            custom => Self::Action {
                action_type: custom.to_string(),
                object_id: required_action_field(fields, "object_id")?,
                target_id: optional_action_field(fields, "target_id")?,
                payload: command_action_payload(fields)?,
            },
        })
    }
}

const MOVE_ACTION_FIELDS: &[&str] = &["type", "object_id", "direction"];
const HARVEST_ACTION_FIELDS: &[&str] = &["type", "object_id", "target_id", "resource"];
const TRANSFER_ACTION_FIELDS: &[&str] = &["type", "object_id", "target_id", "resource", "amount"];
const CLAIM_CONTROLLER_ACTION_FIELDS: &[&str] = &["type", "object_id", "target_id"];
const SPAWN_ACTION_FIELDS: &[&str] = &["type", "object_id", "spawn_id", "body_parts"];
const RECYCLE_ACTION_FIELDS: &[&str] = &["type", "object_id"];
const BUILD_ACTION_FIELDS: &[&str] = &["type", "object_id", "x", "y", "structure"];
const TARGET_ACTION_FIELDS: &[&str] = &["type", "object_id", "target_id"];
const GLOBAL_TRANSFER_ACTION_FIELDS: &[&str] = &["type", "resource", "amount"];
const ALLIED_TRANSFER_ACTION_FIELDS: &[&str] = &["type", "target_player", "resource", "amount"];
const CONCRETE_ACTION_RESERVED_FIELDS: &[&str] = &["type", "object_id", "target_id"];

fn required_exact_action_field<T, E>(
    fields: &serde_json::Map<String, serde_json::Value>,
    allowed_fields: &'static [&'static str],
    field: &'static str,
) -> Result<T, E>
where
    T: DeserializeOwned,
    E: serde::de::Error,
{
    ensure_exact_action_fields(fields, allowed_fields)?;
    required_action_field(fields, field)
}

fn ensure_exact_action_fields<E>(
    fields: &serde_json::Map<String, serde_json::Value>,
    allowed_fields: &'static [&'static str],
) -> Result<(), E>
where
    E: serde::de::Error,
{
    for key in fields.keys() {
        if !allowed_fields.contains(&key.as_str()) {
            return Err(E::unknown_field(key, allowed_fields));
        }
    }
    Ok(())
}

fn required_action_field<T, E>(
    fields: &serde_json::Map<String, serde_json::Value>,
    field: &'static str,
) -> Result<T, E>
where
    T: DeserializeOwned,
    E: serde::de::Error,
{
    let value = fields.get(field).ok_or_else(|| E::missing_field(field))?;
    serde_json::from_value(value.clone()).map_err(E::custom)
}

fn optional_action_field<T, E>(
    fields: &serde_json::Map<String, serde_json::Value>,
    field: &'static str,
) -> Result<Option<T>, E>
where
    T: DeserializeOwned,
    E: serde::de::Error,
{
    fields
        .get(field)
        .map(|value| serde_json::from_value(value.clone()).map_err(E::custom))
        .transpose()
}

fn command_action_payload<E>(
    fields: &serde_json::Map<String, serde_json::Value>,
) -> Result<serde_json::Value, E>
where
    E: serde::de::Error,
{
    let mut payload = serde_json::Map::new();
    for (key, value) in fields {
        if CONCRETE_ACTION_RESERVED_FIELDS.contains(&key.as_str()) {
            continue;
        }
        if matches!(key.as_str(), "payload" | "action_type" | "action_name") {
            return Err(E::custom(format!(
                "action payload must use flattened non-reserved fields; nested or reserved field {key} is not allowed"
            )));
        }
        payload.insert(key.clone(), value.clone());
    }
    Ok(serde_json::Value::Object(payload))
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
            Self::Action {
                action_type,
                object_id,
                target_id,
                payload,
            } => {
                map.serialize_entry("type", action_type)?;
                map.serialize_entry("object_id", object_id)?;
                if let Some(target_id) = target_id {
                    map.serialize_entry("target_id", target_id)?;
                }
                serialize_action_payload_entries(&mut map, payload)?;
            }
            Self::ClaimController {
                object_id,
                target_id,
            } => {
                map.serialize_entry("type", "ClaimController")?;
                map.serialize_entry("object_id", object_id)?;
                map.serialize_entry("target_id", target_id)?;
            }
            Self::Spawn {
                object_id,
                spawn_id,
                body_parts,
            } => {
                map.serialize_entry("type", "Spawn")?;
                map.serialize_entry("object_id", object_id)?;
                map.serialize_entry("spawn_id", spawn_id)?;
                map.serialize_entry("body_parts", body_parts)?;
            }
            Self::Recycle { object_id } => {
                map.serialize_entry("type", "Recycle")?;
                map.serialize_entry("object_id", object_id)?;
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
            Self::Repair {
                object_id,
                target_id,
            } => serialize_target_action(&mut map, "Repair", object_id, target_id)?,
            Self::UpgradeController {
                object_id,
                target_id,
            } => {
                map.serialize_entry("type", "UpgradeController")?;
                map.serialize_entry("object_id", object_id)?;
                map.serialize_entry("target_id", target_id)?;
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
            Self::AlliedTransfer {
                target_player,
                resource,
                amount,
            } => {
                map.serialize_entry("type", "AlliedTransfer")?;
                map.serialize_entry("target_player", target_player)?;
                map.serialize_entry("resource", resource)?;
                map.serialize_entry("amount", amount)?;
            }
        }
        map.end()
    }
}

fn serialize_action_payload_entries<S>(
    map: &mut S,
    payload: &serde_json::Value,
) -> Result<(), S::Error>
where
    S: SerializeMap,
{
    if let Some(payload) = payload.as_object() {
        for (key, value) in payload {
            if matches!(
                key.as_str(),
                "type" | "object_id" | "target_id" | "payload" | "action_type" | "action_name"
            ) {
                return Err(serde::ser::Error::custom(format!(
                    "action payload must not redefine reserved wire field {key}"
                )));
            }
            map.serialize_entry(key, value)?;
        }
    } else if !payload.is_null() {
        return Err(serde::ser::Error::custom(
            "action payload must be an object with flattened wire fields",
        ));
    }
    Ok(())
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
pub struct AdminCredentialProvenance {
    credential_id: String,
    credential_fingerprint: String,
    auth_mode: String,
    admin_identity: String,
    canonical_scopes: Vec<String>,
}

impl AdminCredentialProvenance {
    pub fn new<I, S>(
        credential_id: impl Into<String>,
        credential_fingerprint: impl Into<String>,
        auth_mode: impl Into<String>,
        admin_identity: impl Into<String>,
        scopes: I,
    ) -> Self
    where
        I: IntoIterator<Item = S>,
        S: AsRef<str>,
    {
        Self {
            credential_id: credential_id.into(),
            credential_fingerprint: credential_fingerprint.into(),
            auth_mode: auth_mode.into(),
            admin_identity: admin_identity.into(),
            canonical_scopes: canonical_scope_list(scopes),
        }
    }

    pub fn credential_id(&self) -> &str {
        &self.credential_id
    }

    pub fn credential_fingerprint(&self) -> &str {
        &self.credential_fingerprint
    }

    pub fn auth_mode(&self) -> &str {
        &self.auth_mode
    }

    pub fn admin_identity(&self) -> &str {
        &self.admin_identity
    }

    pub fn canonical_scopes(&self) -> &[String] {
        &self.canonical_scopes
    }

    pub fn has_admin_scope(&self) -> bool {
        self.canonical_scopes
            .iter()
            .any(|scope| scope == ADMIN_SCOPE)
    }

    fn is_valid_for_admin(&self) -> bool {
        !self.credential_id.trim().is_empty()
            && !self.credential_fingerprint.trim().is_empty()
            && self.auth_mode == "admin_cert"
            && !self.admin_identity.trim().is_empty()
            && self.has_admin_scope()
    }
}

fn canonical_scope_list<I, S>(scopes: I) -> Vec<String>
where
    I: IntoIterator<Item = S>,
    S: AsRef<str>,
{
    scopes
        .into_iter()
        .flat_map(|scope| {
            scope
                .as_ref()
                .split_whitespace()
                .map(str::to_string)
                .collect::<Vec<_>>()
        })
        .filter(|scope| !scope.is_empty())
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect()
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CommandAuth {
    pub source: CommandSource,
    pub player_id: PlayerId,
    pub tick_submitted: Tick,
    pub tick_target: Tick,
    admin_credential_provenance: Option<AdminCredentialProvenance>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct CommandAuthWire {
    source: CommandSource,
    player_id: PlayerId,
    tick_submitted: Tick,
    tick_target: Tick,
    #[serde(default)]
    admin_credential_provenance: Option<AdminCredentialProvenance>,
}

impl Serialize for CommandAuth {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        let provenance = if self.source == CommandSource::Admin {
            let provenance = self.admin_credential_provenance.as_ref().ok_or_else(|| {
                serde::ser::Error::custom(
                    "Admin CommandAuth credentials require persisted provenance",
                )
            })?;
            if !provenance.is_valid_for_admin() {
                return Err(serde::ser::Error::custom(
                    "Admin CommandAuth provenance must include valid admin credential metadata",
                ));
            }
            Some(provenance)
        } else {
            None
        };

        let mut map = serializer.serialize_map(Some(if provenance.is_some() { 5 } else { 4 }))?;
        map.serialize_entry("source", &self.source)?;
        map.serialize_entry("player_id", &self.player_id)?;
        map.serialize_entry("tick_submitted", &self.tick_submitted)?;
        map.serialize_entry("tick_target", &self.tick_target)?;
        if let Some(provenance) = provenance {
            map.serialize_entry("admin_credential_provenance", &provenance)?;
        }
        map.end()
    }
}

impl<'de> Deserialize<'de> for CommandAuth {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let wire = CommandAuthWire::deserialize(deserializer)?;
        let auth = Self {
            source: wire.source,
            player_id: wire.player_id,
            tick_submitted: wire.tick_submitted,
            tick_target: wire.tick_target,
            admin_credential_provenance: wire.admin_credential_provenance,
        };
        match (auth.source, auth.admin_credential_provenance.as_ref()) {
            (CommandSource::Admin, Some(provenance)) if provenance.is_valid_for_admin() => {}
            (CommandSource::Admin, Some(_)) => {
                return Err(serde::de::Error::custom(
                    "Admin CommandAuth provenance must include admin_cert metadata and swarm:admin scope",
                ));
            }
            (CommandSource::Admin, None) => {
                return Err(serde::de::Error::custom(
                    "Admin CommandAuth credentials require persisted provenance",
                ));
            }
            (_, Some(_)) => {
                return Err(serde::de::Error::custom(
                    "admin_credential_provenance is only valid for Admin CommandAuth",
                ));
            }
            (_, None) => {}
        }
        Ok(auth)
    }
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
pub(crate) struct ValidatedCommand {
    raw: RawCommand,
    effective_player_id: PlayerId,
}

mod world_mutate_sealed {
    pub trait Sealed {}
    impl Sealed for super::RawCommand {}
}

pub(crate) trait WorldMutate: world_mutate_sealed::Sealed {
    fn validate_and_apply(self, world: &mut World) -> CommandResult;
}

impl WorldMutate for RawCommand {
    fn validate_and_apply(self, world: &mut World) -> CommandResult {
        let validated = validate_command(world, self)?;
        apply_command(world, validated)
    }
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
    AlreadyActed,
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
    TargetTransferLocked,
    DailyTransferCapExceeded,
    TargetFuelTooLow,
    FuelExhausted,
    SafeModeActive,
    TargetFortifyCooldown,
    TargetOverloadCooldown,
    DisruptedResisted {
        part: BodyPart,
    },
    RateLimited,
    InternalError,
    ServerOverloaded,
    SnapshotOverBudget,
    UnknownAction {
        action: String,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum AuthError {
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
}

pub const CANONICAL_REJECTION_REASONS: &[&str] = &[
    "InvalidJson",
    "SchemaViolation",
    "ObjectNotFound",
    "NotOwner",
    "InsufficientResource",
    "OutOfRange",
    "NotStructure",
    "NotController",
    "NotVisibleOrNotFound",
    "TargetNotVisible",
    "SpawnOnCooldown",
    "RoomDroneCapReached",
    "AuthContextInvalid",
    "CooldownActive",
    "InvalidDirection",
    "PositionOccupied",
    "ConstructionLimitReached",
    "SafeModeActive",
    "TargetOverloadCooldown",
    "TargetFortifyCooldown",
    "NotEnoughBodyParts",
    "InvalidBodyPart",
    "InvalidStructureType",
    "InvalidResourceType",
    "SourceNotAllowed",
    "UnknownAction",
    "GlobalStorageDisabled",
    "TransferInProgress",
    "RateLimited",
    "InvalidCertificate",
    "NotAuthorized",
    "FuelExhausted",
    "TimeoutExceeded",
    "SnapshotOverBudget",
    "CommandBufferFull",
    "ServerOverloaded",
    "InternalError",
];

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

pub(crate) fn source_gate(
    player_id: PlayerId,
    tick: Tick,
    source: CommandSource,
    intent: CommandIntent,
) -> Result<RawCommand, RejectionReason> {
    if source == CommandSource::Admin {
        return Err(RejectionReason::AuthContextInvalid);
    }
    if !source_allows_action(source, &intent.action) {
        return Err(RejectionReason::SourceNotAllowed);
    }

    Ok(RawCommand {
        player_id,
        tick,
        source,
        auth: CommandAuth::server_injected(source, player_id, tick, tick),
        sequence: intent.sequence,
        action: intent.action,
    })
}

#[allow(dead_code)]
pub(crate) fn admin_source_gate(
    player_id: PlayerId,
    tick: Tick,
    intent: CommandIntent,
    provenance: AdminCredentialProvenance,
) -> Result<RawCommand, RejectionReason> {
    if !source_allows_action(CommandSource::Admin, &intent.action) {
        return Err(RejectionReason::SourceNotAllowed);
    }
    Ok(RawCommand {
        player_id,
        tick,
        source: CommandSource::Admin,
        auth: CommandAuth::server_injected_admin(player_id, tick, tick, provenance)?,
        sequence: intent.sequence,
        action: intent.action,
    })
}

pub(crate) fn collect_command_intents(
    player_id: PlayerId,
    tick: Tick,
    source: CommandSource,
    intents: Vec<CommandIntent>,
) -> Result<Vec<RawCommand>, TickValidationError> {
    if intents.len() > MAX_COMMANDS_PER_PLAYER {
        return Err(TickValidationError::TooManyCommands);
    }

    intents
        .into_iter()
        .map(|intent| {
            source_gate(player_id, tick, source, intent)
                .map_err(|_| TickValidationError::SchemaViolation)
        })
        .collect()
}

pub fn sort_raw_commands(commands: &mut [RawCommand]) {
    sort_raw_commands_with_seed(commands, 0);
}

pub fn sort_raw_commands_with_seed(commands: &mut [RawCommand], world_seed: u64) {
    let active_players = players_from_commands(commands);
    sort_raw_commands_for_active_players(commands, world_seed, &active_players);
}

pub fn sort_raw_commands_for_active_players(
    commands: &mut [RawCommand],
    world_seed: u64,
    active_players: &[PlayerId],
) {
    let shuffle_indices = seeded_shuffle_indices(commands, world_seed, active_players);
    commands.sort_by_key(|command| {
        let priority_class = command_priority_class(command.source);
        (
            priority_class,
            shuffle_indices
                .get(&(priority_class, command.tick, command.player_id))
                .copied()
                .unwrap_or(usize::MAX),
            command_source_rank(command.source),
            command.sequence,
            command_hash(command),
        )
    });
}

fn players_from_commands(commands: &[RawCommand]) -> Vec<PlayerId> {
    let mut players = commands
        .iter()
        .map(|command| command.player_id)
        .collect::<Vec<_>>();
    players.sort_unstable();
    players.dedup();
    players
}

fn seeded_shuffle_indices(
    commands: &[RawCommand],
    world_seed: u64,
    active_players: &[PlayerId],
) -> IndexMap<(u8, Tick, PlayerId), usize> {
    let mut buckets = Vec::<(u8, Tick)>::new();
    for command in commands {
        let bucket = (command_priority_class(command.source), command.tick);
        if !buckets.contains(&bucket) {
            buckets.push(bucket);
        }
    }

    let sorted_active_players = sorted_unique_players(active_players);
    let mut indices = IndexMap::new();
    for (priority_class, tick) in buckets {
        let mut players = sorted_active_players.clone();
        if players.is_empty() {
            players = players_from_bucket(commands, priority_class, tick);
        }
        seeded_shuffle_players(&mut players, world_seed, tick);
        for (index, player_id) in players.into_iter().enumerate() {
            indices.insert((priority_class, tick, player_id), index);
        }
    }

    indices
}

fn sorted_unique_players(players: &[PlayerId]) -> Vec<PlayerId> {
    let mut players = players.to_vec();
    players.sort_unstable();
    players.dedup();
    players
}

fn players_from_bucket(commands: &[RawCommand], priority_class: u8, tick: Tick) -> Vec<PlayerId> {
    let mut players = commands
        .iter()
        .filter(|command| {
            command_priority_class(command.source) == priority_class && command.tick == tick
        })
        .map(|command| command.player_id)
        .collect::<Vec<_>>();
    players.sort_unstable();
    players.dedup();
    players
}

fn seeded_shuffle_players(players: &mut [PlayerId], world_seed: u64, tick: Tick) {
    let mut hasher = blake3::Hasher::new();
    hasher.update(b"shuffle");
    hasher.update(&world_seed.to_le_bytes());
    hasher.update(&tick.to_le_bytes());
    let mut reader = hasher.finalize_xof();

    for i in 0..players.len() {
        let remaining = players.len() - i;
        let offset = loop {
            let mut bytes = [0_u8; 8];
            reader.fill(&mut bytes);
            if let Some(offset) = unbiased_shuffle_offset(u64::from_le_bytes(bytes), remaining) {
                break offset;
            }
        };
        players.swap(i, i + offset);
    }
}

fn unbiased_shuffle_offset(sample: u64, remaining: usize) -> Option<usize> {
    let bound = u64::try_from(remaining).ok()?;
    if bound == 0 {
        return None;
    }
    let unbiased_limit = u64::MAX - (u64::MAX % bound);
    (sample < unbiased_limit).then_some((sample % bound) as usize)
}

fn command_priority_class(source: CommandSource) -> u8 {
    match source {
        CommandSource::Admin => 0,
        CommandSource::Wasm | CommandSource::TestHarness | CommandSource::Tutorial => 1,
        CommandSource::McpDeploy | CommandSource::Deploy => 2,
        CommandSource::McpQuery => 3,
        CommandSource::Replay => 4,
        CommandSource::Rollback => 5,
        CommandSource::RuleMod => 6,
        CommandSource::Simulate => 7,
        CommandSource::DryRun => 8,
    }
}

fn command_source_rank(source: CommandSource) -> u8 {
    match source {
        CommandSource::Admin => 0,
        CommandSource::Wasm => 1,
        CommandSource::McpDeploy => 2,
        CommandSource::McpQuery => 3,
        CommandSource::Replay => 4,
        CommandSource::TestHarness => 5,
        CommandSource::Tutorial => 6,
        CommandSource::Deploy => 7,
        CommandSource::Rollback => 8,
        CommandSource::RuleMod => 9,
        CommandSource::Simulate => 10,
        CommandSource::DryRun => 11,
    }
}

fn command_hash(command: &RawCommand) -> [u8; 32] {
    let bytes = serde_json::to_vec(command).unwrap_or_default();
    *blake3::hash(&bytes).as_bytes()
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

    let intents = commands
        .iter()
        .map(|command| {
            serde_json::from_value(command.clone())
                .map_err(|_| TickValidationError::SchemaViolation)
        })
        .collect::<Result<Vec<CommandIntent>, TickValidationError>>()?;
    collect_command_intents(player_id, tick, CommandSource::Wasm, intents)
}

pub fn object_id(entity: Entity) -> ObjectId {
    entity.to_bits()
}

fn effective_command_player_id(world: &World, raw: &RawCommand) -> PlayerId {
    if raw.source != CommandSource::Admin {
        return raw.player_id;
    }
    let actor_id = match &raw.action {
        CommandAction::Move { object_id, .. }
        | CommandAction::Harvest { object_id, .. }
        | CommandAction::Transfer { object_id, .. }
        | CommandAction::Withdraw { object_id, .. }
        | CommandAction::Action { object_id, .. }
        | CommandAction::ClaimController { object_id, .. }
        | CommandAction::Recycle { object_id }
        | CommandAction::Build { object_id, .. }
        | CommandAction::Repair { object_id, .. }
        | CommandAction::UpgradeController { object_id, .. } => Some(*object_id),
        CommandAction::Spawn { object_id, .. } => Some(*object_id),
        CommandAction::TransferToGlobal { .. }
        | CommandAction::TransferFromGlobal { .. }
        | CommandAction::AlliedTransfer { .. } => None,
    };
    actor_id
        .and_then(|object_id| entity(object_id).ok())
        .and_then(|entity| world.get_entity(entity).ok())
        .and_then(|entity| {
            entity
                .get::<Drone>()
                .map(|drone| drone.owner)
                .or_else(|| {
                    entity
                        .get::<Structure>()
                        .and_then(|structure| structure.owner)
                })
                .or_else(|| {
                    entity
                        .get::<Controller>()
                        .and_then(|controller| controller.owner)
                })
        })
        .unwrap_or(raw.player_id)
}

pub(crate) fn validate_command(
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
    let effective_player_id = effective_command_player_id(world, &raw);

    let result = match &raw.action {
        CommandAction::Move {
            object_id,
            direction,
        } => validate_move(world, effective_player_id, raw.tick, *object_id, *direction),
        CommandAction::Harvest {
            object_id,
            target_id,
            resource: _,
        } => validate_harvest(world, effective_player_id, raw.tick, *object_id, *target_id),
        CommandAction::Transfer {
            object_id,
            target_id,
            resource,
            amount,
        } => validate_transfer(
            world,
            effective_player_id,
            *object_id,
            *target_id,
            resource,
            *amount,
            raw.tick,
        ),
        CommandAction::Withdraw {
            object_id,
            target_id,
            resource,
            amount,
        } => validate_withdraw(
            world,
            effective_player_id,
            *object_id,
            *target_id,
            resource,
            *amount,
            raw.tick,
        ),
        CommandAction::Action {
            action_type,
            object_id,
            target_id,
            payload,
        } => validate_action(
            world,
            effective_player_id,
            raw.tick,
            action_type,
            *object_id,
            *target_id,
            payload,
        ),
        CommandAction::ClaimController {
            object_id,
            target_id,
        } => {
            validate_claim_controller(world, effective_player_id, raw.tick, *object_id, *target_id)
        }
        CommandAction::Spawn {
            object_id,
            spawn_id,
            body_parts,
        } => validate_spawn_drone(
            world,
            effective_player_id,
            *object_id,
            *spawn_id,
            body_parts,
        ),
        CommandAction::Recycle { object_id } => {
            validate_recycle(world, effective_player_id, raw.tick, *object_id)
        }
        CommandAction::Build {
            object_id,
            x,
            y,
            structure,
        } => validate_build(
            world,
            effective_player_id,
            raw.tick,
            *object_id,
            *x,
            *y,
            *structure,
        ),
        CommandAction::Repair {
            object_id,
            target_id,
        } => validate_repair(world, effective_player_id, raw.tick, *object_id, *target_id),
        CommandAction::UpgradeController {
            object_id,
            target_id,
        } => validate_upgrade_controller(
            world,
            effective_player_id,
            raw.tick,
            *object_id,
            *target_id,
        ),
        CommandAction::TransferToGlobal { resource, amount } => {
            validate_transfer_to_global(world, effective_player_id, resource, *amount)
        }
        CommandAction::TransferFromGlobal { resource, amount } => {
            validate_transfer_from_global(world, effective_player_id, resource, *amount)
        }
        CommandAction::AlliedTransfer {
            target_player,
            resource,
            amount,
        } => validate_allied_transfer(
            world,
            effective_player_id,
            *target_player,
            resource,
            *amount,
        ),
    };

    if matches!(result, Err(RejectionReason::InsufficientResource { .. })) {
        send_onboarding_event(
            world,
            OnboardingEvent::ResourceBottleneckExplanationAvailable,
        );
    }
    result?;

    Ok(ValidatedCommand {
        raw,
        effective_player_id,
    })
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

fn action_uses_global_storage(action: &CommandAction) -> bool {
    matches!(
        action,
        CommandAction::TransferToGlobal { .. }
            | CommandAction::TransferFromGlobal { .. }
            | CommandAction::AlliedTransfer { .. }
    )
}

impl CommandAuth {
    pub(crate) fn server_injected(
        source: CommandSource,
        player_id: PlayerId,
        tick_submitted: Tick,
        tick_target: Tick,
    ) -> Self {
        assert_ne!(
            source,
            CommandSource::Admin,
            "Admin CommandAuth requires credential provenance"
        );
        Self {
            source,
            player_id,
            tick_submitted,
            tick_target,
            admin_credential_provenance: None,
        }
    }

    #[allow(dead_code)]
    pub(crate) fn server_injected_admin(
        player_id: PlayerId,
        tick_submitted: Tick,
        tick_target: Tick,
        provenance: AdminCredentialProvenance,
    ) -> Result<Self, RejectionReason> {
        if !provenance.is_valid_for_admin() {
            return Err(RejectionReason::AuthContextInvalid);
        }
        Ok(Self {
            source: CommandSource::Admin,
            player_id,
            tick_submitted,
            tick_target,
            admin_credential_provenance: Some(provenance),
        })
    }

    pub fn admin_credential_provenance(&self) -> Option<&AdminCredentialProvenance> {
        self.admin_credential_provenance.as_ref()
    }

    pub fn has_valid_admin_credential_provenance(&self) -> bool {
        self.admin_credential_provenance()
            .is_some_and(|provenance| provenance.is_valid_for_admin())
    }

    fn matches_raw_envelope(&self, raw: &RawCommand) -> bool {
        let envelope_matches = self.source == raw.source
            && self.player_id == raw.player_id
            && self.tick_target == raw.tick
            && self.tick_submitted <= self.tick_target;
        if !envelope_matches {
            return false;
        }
        if raw.source == CommandSource::Admin {
            self.has_valid_admin_credential_provenance()
        } else {
            true
        }
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
        builtin_action_metadata(
            "Action",
            "Dispatch a registered combat or effect action",
            &["action_type", "object_id", "target_id", "payload"],
        ),
        builtin_action_metadata(
            "ClaimController",
            "Claim a room controller",
            &["object_id", "target_id"],
        ),
        builtin_action_metadata(
            "Spawn",
            "Spawn a drone",
            &["object_id", "spawn_id", "body_parts"],
        ),
        builtin_action_metadata(
            "Repair",
            "Repair a target structure",
            &["object_id", "target_id"],
        ),
        builtin_action_metadata(
            "UpgradeController",
            "Upgrade a room controller",
            &["object_id", "target_id"],
        ),
        builtin_action_metadata(
            "Recycle",
            "Recycle a drone for a body cost refund",
            &["object_id"],
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
        builtin_action_metadata(
            "AlliedTransfer",
            "Transfer resources to an allied player (200bp fee, 200 tick delay)",
            &["target_player", "resource", "amount"],
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
        CommandAction::Action { action_type, .. } => action_type,
        CommandAction::ClaimController { .. } => "ClaimController",
        CommandAction::Spawn { .. } => "Spawn",
        CommandAction::Recycle { .. } => "Recycle",
        CommandAction::Build { .. } => "Build",
        CommandAction::Repair { .. } => "Repair",
        CommandAction::UpgradeController { .. } => "UpgradeController",
        CommandAction::TransferToGlobal { .. } => "TransferToGlobal",
        CommandAction::TransferFromGlobal { .. } => "TransferFromGlobal",
        CommandAction::AlliedTransfer { .. } => "AlliedTransfer",
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
            CommandAction::Spawn {
                object_id,
                spawn_id,
                body_parts,
            } => serde_json::json!({
                "reason": "TileOccupied",
                "action": action,
                "conflict": "first_come_first_served",
                "refund_policy": { "fuel_percent": 50 },
                "object_id": object_id,
                "spawn_id": spawn_id,
                "body_parts": body_parts,
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
        fields.insert(
            "reason".to_string(),
            serde_json::Value::String(canonical_reason.to_string()),
        );
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

pub(crate) fn canonical_rejection_reason(rejection: &RejectionReason) -> &'static str {
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
        | RejectionReason::AlreadyActed
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
        RejectionReason::TargetTransferLocked => "RateLimited",
        RejectionReason::DailyTransferCapExceeded => "RateLimited",
        RejectionReason::PlayerNotFound => "NotVisibleOrNotFound",
        RejectionReason::FuelExhausted => "FuelExhausted",
        RejectionReason::SafeModeActive => "SafeModeActive",
        RejectionReason::TargetFortifyCooldown => "TargetFortifyCooldown",
        RejectionReason::TargetOverloadCooldown => "TargetOverloadCooldown",
        RejectionReason::DisruptedResisted { .. } => "NotEnoughBodyParts",
        RejectionReason::RateLimited => "RateLimited",
        RejectionReason::InternalError => "InternalError",
        RejectionReason::ServerOverloaded => "ServerOverloaded",
        RejectionReason::SnapshotOverBudget => "SnapshotOverBudget",
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
            | RejectionReason::RateLimited
            | RejectionReason::InternalError
            | RejectionReason::ServerOverloaded
            | RejectionReason::SnapshotOverBudget
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

pub(crate) fn apply_command(world: &mut World, command: ValidatedCommand) -> CommandResult {
    let action_tick = command.raw.tick;
    let player_id = command.effective_player_id;
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
        } => apply_transfer(world, object_id, target_id, &resource, amount),
        CommandAction::Withdraw {
            object_id,
            target_id,
            resource,
            amount,
        } => apply_withdraw(world, object_id, target_id, &resource, amount),
        CommandAction::Action {
            action_type,
            object_id,
            target_id,
            payload,
        } => {
            actor_id = Some(object_id);
            apply_action(
                world,
                player_id,
                action_tick,
                &action_type,
                object_id,
                target_id,
                &payload,
            )
        }
        CommandAction::ClaimController {
            object_id,
            target_id,
        } => {
            actor_id = Some(object_id);
            apply_claim_controller(world, player_id, target_id)
        }
        CommandAction::Spawn {
            object_id,
            spawn_id,
            body_parts,
        } => {
            actor_id = Some(object_id);
            apply_spawn_drone(world, player_id, spawn_id, body_parts)
        }
        CommandAction::Recycle { object_id } => {
            apply_recycle(world, player_id, action_tick, object_id)
        }
        CommandAction::Build {
            object_id,
            x,
            y,
            structure,
        } => {
            actor_id = Some(object_id);
            apply_build(world, player_id, object_id, x, y, structure)
        }
        CommandAction::Repair {
            object_id,
            target_id,
        } => {
            actor_id = Some(object_id);
            apply_repair(world, object_id, target_id)
        }
        CommandAction::UpgradeController {
            object_id,
            target_id,
        } => {
            actor_id = Some(object_id);
            apply_upgrade_controller(world, object_id, target_id)
        }
        CommandAction::TransferToGlobal { resource, amount } => {
            apply_transfer_to_global(world, player_id, &resource, amount)
        }
        CommandAction::TransferFromGlobal { resource, amount } => {
            apply_transfer_from_global(world, player_id, &resource, amount)
        }
        CommandAction::AlliedTransfer {
            target_player,
            resource,
            amount,
        } => apply_allied_transfer(world, player_id, target_player, &resource, amount),
    };

    if result.is_ok()
        && let Some(object_id) = actor_id
    {
        mark_drone_action(world, object_id, action_tick);
    }

    result
}

fn mark_drone_action(world: &mut World, object_id: ObjectId, tick: Tick) {
    if let Ok(entity) = entity(object_id)
        && let Some(mut drone) = world.entity_mut(entity).get_mut::<Drone>()
    {
        drone.last_action_tick = tick;
    }
}

fn validate_move(
    world: &mut World,
    player_id: PlayerId,
    tick: Tick,
    object_id: ObjectId,
    direction: Direction,
) -> CommandResult {
    let (position, drone) = drone_snapshot(world, object_id)?;
    ensure_owner(&drone, player_id)?;
    ensure_drone_can_take_main_action(&drone, tick, BodyPart::Move, true)?;
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
    tick: Tick,
    object_id: ObjectId,
    target_id: ObjectId,
) -> CommandResult {
    let (position, drone) = drone_snapshot(world, object_id)?;
    ensure_owner(&drone, player_id)?;
    ensure_drone_can_take_main_action(&drone, tick, BodyPart::Work, true)?;
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
    _tick: Tick,
) -> CommandResult {
    let (position, drone) = drone_snapshot(world, object_id)?;
    ensure_owner(&drone, player_id)?;
    ensure_drone_can_act(&drone, BodyPart::Carry, false)?;
    let available = *drone.carry.get(resource).unwrap_or(&0);
    if available < amount {
        return Err(RejectionReason::InsufficientResource {
            resource: resource.to_string(),
            required: amount,
            available,
        });
    }

    if let Ok((_, controller)) = controller_snapshot(world, target_id)
        && controller.owner != Some(player_id)
    {
        return Err(RejectionReason::NotOwner);
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
    _tick: Tick,
) -> CommandResult {
    let (position, drone) = drone_snapshot(world, object_id)?;
    ensure_owner(&drone, player_id)?;
    ensure_drone_can_act(&drone, BodyPart::Carry, false)?;
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
    tick: Tick,
    object_id: ObjectId,
    target_id: ObjectId,
) -> CommandResult {
    let (position, drone) = drone_snapshot(world, object_id)?;
    ensure_owner(&drone, player_id)?;
    ensure_drone_can_take_main_action(&drone, tick, BodyPart::Attack, true)?;
    let (target_position, target_owner) = attackable_snapshot(world, target_id)?;
    if target_owner == Some(player_id) {
        return Err(RejectionReason::FriendlyTarget);
    }
    ensure_range(position, target_position, 1)
}

fn validate_ranged_attack(
    world: &mut World,
    player_id: PlayerId,
    tick: Tick,
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
    ensure_drone_can_take_main_action(&drone, tick, BodyPart::RangedAttack, true)?;
    let (target_position, target_owner) = attackable_snapshot(world, target_id)?;
    if target_owner == Some(player_id) {
        return Err(RejectionReason::FriendlyTarget);
    }
    ensure_range(position, target_position, range)
}

fn validate_heal(
    world: &mut World,
    player_id: PlayerId,
    tick: Tick,
    object_id: ObjectId,
    target_id: ObjectId,
) -> CommandResult {
    let (position, drone) = drone_snapshot(world, object_id)?;
    ensure_owner(&drone, player_id)?;
    ensure_drone_can_take_main_action(&drone, tick, BodyPart::Heal, false)?;
    let (target_position, target) = drone_snapshot(world, target_id)?;
    if target.owner != player_id {
        return Err(RejectionReason::NotFriendly);
    }
    if target.hits >= target.hits_max {
        return Err(RejectionReason::AlreadyFullHealth);
    }
    ensure_range(position, target_position, 3)
}

fn validate_action(
    world: &mut World,
    player_id: PlayerId,
    tick: Tick,
    action_type: &str,
    object_id: ObjectId,
    target_id: Option<ObjectId>,
    payload: &serde_json::Value,
) -> CommandResult {
    match action_type {
        "Attack" => validate_attack(
            world,
            player_id,
            tick,
            object_id,
            require_target_id(target_id)?,
        ),
        "RangedAttack" => validate_ranged_attack(
            world,
            player_id,
            tick,
            object_id,
            require_target_id(target_id)?,
            payload_u32(payload, "range").unwrap_or(MAX_RANGED_ATTACK_RANGE),
        ),
        "Heal" => validate_heal(
            world,
            player_id,
            tick,
            object_id,
            require_target_id(target_id)?,
        ),
        custom => validate_custom_action(world, player_id, tick, custom, object_id, target_id),
    }
}

fn require_target_id(target_id: Option<ObjectId>) -> Result<ObjectId, RejectionReason> {
    target_id.ok_or(RejectionReason::ObjectNotFound)
}

fn payload_u32(payload: &serde_json::Value, field: &str) -> Option<u32> {
    payload
        .get(field)
        .and_then(serde_json::Value::as_u64)
        .and_then(|value| u32::try_from(value).ok())
}

fn validate_claim_controller(
    world: &mut World,
    player_id: PlayerId,
    tick: Tick,
    object_id: ObjectId,
    controller_id: ObjectId,
) -> CommandResult {
    let (position, drone) = drone_snapshot(world, object_id)?;
    ensure_owner(&drone, player_id)?;
    ensure_drone_can_take_main_action(&drone, tick, BodyPart::Claim, true)?;
    let (target_position, controller) = controller_snapshot(world, controller_id)?;
    if controller.owner.is_some() && controller.owner != Some(player_id) {
        return Err(RejectionReason::NotOwner);
    }
    ensure_range(position, target_position, 1)
}

fn validate_spawn_drone(
    world: &mut World,
    player_id: PlayerId,
    object_id: ObjectId,
    spawn_id: ObjectId,
    body: &[BodyPart],
) -> CommandResult {
    let (_, drone) = drone_snapshot(world, object_id)?;
    ensure_owner(&drone, player_id)?;
    let (position, structure) = structure_snapshot(world, spawn_id)?;
    if structure.structure_type != StructureType::SPAWN || structure.owner != Some(player_id) {
        return Err(RejectionReason::NotYourSpawn);
    }
    if structure.cooldown > 0 {
        return Err(RejectionReason::SpawnOnCooldown);
    }
    if body.is_empty() {
        return Err(RejectionReason::InvalidBodyPart);
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
    tick: Tick,
    object_id: ObjectId,
) -> CommandResult {
    let (_, drone) = drone_snapshot(world, object_id)?;
    ensure_owner(&drone, player_id)?;
    ensure_drone_main_action_available(&drone, tick)
}

fn validate_build(
    world: &mut World,
    player_id: PlayerId,
    tick: Tick,
    object_id: ObjectId,
    x: i32,
    y: i32,
    structure: StructureType,
) -> CommandResult {
    let (position, drone) = drone_snapshot(world, object_id)?;
    ensure_owner(&drone, player_id)?;
    ensure_drone_can_take_main_action(&drone, tick, BodyPart::Work, true)?;
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

fn validate_repair(
    world: &mut World,
    player_id: PlayerId,
    tick: Tick,
    object_id: ObjectId,
    target_id: ObjectId,
) -> CommandResult {
    let (position, drone) = drone_snapshot(world, object_id)?;
    ensure_owner(&drone, player_id)?;
    ensure_drone_can_take_main_action(&drone, tick, BodyPart::Work, false)?;
    let (target_position, structure) = structure_snapshot(world, target_id)?;
    if structure.owner.is_some() && structure.owner != Some(player_id) {
        return Err(RejectionReason::NotOwner);
    }
    if structure.hits >= structure.hits_max {
        return Err(RejectionReason::AlreadyFullHealth);
    }
    ensure_range(position, target_position, 3)
}

fn validate_upgrade_controller(
    world: &mut World,
    player_id: PlayerId,
    tick: Tick,
    object_id: ObjectId,
    controller_id: ObjectId,
) -> CommandResult {
    let (position, drone) = drone_snapshot(world, object_id)?;
    ensure_owner(&drone, player_id)?;
    ensure_drone_can_take_main_action(&drone, tick, BodyPart::Work, false)?;
    let (target_position, controller) = controller_snapshot(world, controller_id)?;
    if controller.owner != Some(player_id) {
        return Err(RejectionReason::NotOwner);
    }
    ensure_range(position, target_position, 3)
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
    ensure_drone_main_action_available(&drone, tick)?;
    validate_special_action_requirements(world, &drone, &action)?;
    if custom_action_on_cooldown(world, object_id, action_type, tick) {
        return Err(RejectionReason::CooldownActive);
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
    let (_, source_drone) = drone_snapshot(world, object_id)?;
    let target_player = resource_target_player(world, target_id);
    let object = entity(object_id)?;
    let target = entity(target_id)?;
    take_from_drone(world, object, resource, amount);
    add_to_target(world, target, resource, amount)?;
    record_resource_flow(
        world,
        Some(source_drone.owner),
        target_player,
        resource,
        amount,
        ResourceOperation::LocalTransfer,
        0,
        0,
    );
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
    let (_, drone) = drone_snapshot(world, object_id)?;
    let source_player = resource_target_player(world, target_id);
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
    record_resource_flow(
        world,
        source_player,
        Some(drone.owner),
        resource,
        amount,
        ResourceOperation::LocalTransfer,
        0,
        0,
    );
    Ok(())
}

fn apply_action(
    world: &mut World,
    player_id: PlayerId,
    tick: Tick,
    action_type: &str,
    object_id: ObjectId,
    target_id: Option<ObjectId>,
    payload: &serde_json::Value,
) -> CommandResult {
    match action_type {
        "Attack" => apply_basic_attack(world, object_id, require_target_id(target_id)?),
        "RangedAttack" => {
            apply_basic_ranged_attack(world, object_id, require_target_id(target_id)?)
        }
        "Heal" => apply_basic_heal(world, object_id, require_target_id(target_id)?),
        custom => apply_custom_action(
            world,
            player_id,
            tick,
            custom,
            object_id,
            target_id,
            payload_structure(payload),
        ),
    }
}

fn payload_structure(payload: &serde_json::Value) -> Option<StructureType> {
    payload
        .get("structure")
        .cloned()
        .and_then(|value| serde_json::from_value(value).ok())
}

fn apply_basic_attack(
    world: &mut World,
    object_id: ObjectId,
    target_id: ObjectId,
) -> CommandResult {
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

fn apply_basic_ranged_attack(
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
    world
        .resource_mut::<PendingDamage>()
        .push(target, damage, damage_type.to_string());
    Ok(())
}

fn apply_basic_heal(world: &mut World, object_id: ObjectId, target_id: ObjectId) -> CommandResult {
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
    if world.entity(target).get::<Drone>().is_none() {
        return Err(RejectionReason::ObjectNotFound);
    }
    world.resource_mut::<PendingHeal>().push(target, heal);
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
    record_resource_cost(world, player_id, &cost, true, ResourceOperation::SpawnCost);
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
) -> CommandResult {
    let object = entity(object_id)?;
    let (position, drone) = drone_snapshot(world, object_id)?;
    let refund = recycle_refund_cost(world, tick, &drone.body);

    refund_recycle_cost(world, player_id, &refund);
    record_resource_award(world, player_id, &refund, ResourceOperation::RecycleRefund);
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
    record_resource_cost(world, player_id, &cost, false, ResourceOperation::BuildCost);
    let position = Position {
        x,
        y,
        room: position.room,
    };
    let stable_id = world.resource_mut::<StableEntityIdAllocator>().allocate();
    let structure = structure_defaults(structure_type, Some(player_id), world);
    world
        .resource_mut::<PendingEntityCreation>()
        .entries
        .push(PendingEntityCreationEntry {
            stable_id,
            kind: PendingEntityKind::Structure {
                position,
                structure,
            },
        });
    send_onboarding_event(world, OnboardingEvent::StructureBuilt);
    Ok(())
}

fn apply_repair(world: &mut World, object_id: ObjectId, target_id: ObjectId) -> CommandResult {
    let object = entity(object_id)?;
    let target = entity(target_id)?;
    let (_, drone) = drone_snapshot(world, object_id)?;
    let work_parts = drone
        .body
        .iter()
        .filter(|part| **part == BodyPart::Work)
        .count() as u32;
    let energy = drone
        .carry
        .get(ENERGY_RESOURCE)
        .copied()
        .unwrap_or_default();
    let repair_amount = work_parts.max(1).saturating_mul(100).min(energy).min(
        world
            .entity(target)
            .get::<Structure>()
            .ok_or(RejectionReason::ObjectNotFound)?
            .hits_max
            .saturating_sub(
                world
                    .entity(target)
                    .get::<Structure>()
                    .ok_or(RejectionReason::ObjectNotFound)?
                    .hits,
            ),
    );

    if repair_amount == 0 {
        return Ok(());
    }

    take_from_drone(world, object, ENERGY_RESOURCE, repair_amount);
    let mut entity_mut = world.entity_mut(target);
    let mut structure = entity_mut
        .get_mut::<Structure>()
        .ok_or(RejectionReason::ObjectNotFound)?;
    structure.hits = structure
        .hits
        .saturating_add(repair_amount)
        .min(structure.hits_max);
    Ok(())
}

fn apply_upgrade_controller(
    world: &mut World,
    object_id: ObjectId,
    controller_id: ObjectId,
) -> CommandResult {
    let object = entity(object_id)?;
    let controller = entity(controller_id)?;
    let (_, drone) = drone_snapshot(world, object_id)?;
    let work_parts = drone
        .body
        .iter()
        .filter(|part| **part == BodyPart::Work)
        .count() as u32;
    let energy = drone
        .carry
        .get(ENERGY_RESOURCE)
        .copied()
        .unwrap_or_default();
    let amount = work_parts.max(1).min(energy);

    if amount == 0 {
        return Ok(());
    }

    take_from_drone(world, object, ENERGY_RESOURCE, amount);
    world
        .resource_mut::<PendingControllerUpgrade>()
        .0
        .push((controller.to_bits(), amount));
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
    record_resource_cost(world, player_id, &cost, false, ResourceOperation::BuildCost);
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
            let _ = structure_type;
            queue_special_attack(
                world,
                SpecialAttackKind::Fabricate,
                object_id,
                target_id,
                player_id,
                0,
            )?;
        }
        Some("fortify") => {
            let target_id = target_id.unwrap_or(object_id);
            queue_special_attack(
                world,
                SpecialAttackKind::Fortify,
                object_id,
                target_id,
                player_id,
                action.base_damage.unwrap_or_default(),
            )?;
        }
        Some("disrupt") => {
            let Some(target_id) = target_id else {
                return Ok(());
            };
            queue_special_attack(
                world,
                SpecialAttackKind::Disrupt,
                object_id,
                target_id,
                player_id,
                action.base_damage.unwrap_or_default(),
            )?;
        }
        Some("hack") => {
            let Some(target_id) = target_id else {
                return Ok(());
            };
            queue_special_attack(
                world,
                SpecialAttackKind::Hack,
                object_id,
                target_id,
                player_id,
                action.base_damage.unwrap_or_default(),
            )?;
        }
        Some("drain") => {
            let Some(target_id) = target_id else {
                return Ok(());
            };
            queue_special_attack(
                world,
                SpecialAttackKind::Drain,
                object_id,
                target_id,
                player_id,
                action.base_damage.unwrap_or(15),
            )?;
        }
        Some("overload") => {
            let Some(target_id) = target_id else {
                return Ok(());
            };
            queue_special_attack(
                world,
                SpecialAttackKind::Overload,
                object_id,
                target_id,
                player_id,
                action.base_damage.unwrap_or_default(),
            )?;
        }
        Some("debilitate") => {
            let Some(target_id) = target_id else {
                return Ok(());
            };
            queue_special_attack(
                world,
                SpecialAttackKind::Debilitate,
                object_id,
                target_id,
                player_id,
                action.base_damage.unwrap_or_default(),
            )?;
        }
        Some("leech") => {
            let Some(target_id) = target_id else {
                return Ok(());
            };
            queue_special_attack(
                world,
                SpecialAttackKind::Leech,
                object_id,
                target_id,
                player_id,
                action.base_damage.unwrap_or(15),
            )?;
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

fn queue_special_attack(
    world: &mut World,
    kind: SpecialAttackKind,
    source_id: ObjectId,
    target_id: ObjectId,
    owner: PlayerId,
    amount: u32,
) -> CommandResult {
    let source = entity(source_id)?;
    let target = entity(target_id)?;
    world
        .resource_mut::<PendingSpecialAttack>()
        .intents
        .push(StatusActionIntent {
            kind,
            source,
            target,
            owner,
            amount,
        });
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
        Some("disrupt") => {
            crate::systems::body_part_match(drone, &[BodyPart::Attack])
                .map_err(|part| RejectionReason::DisruptedResisted { part })?;
            Ok(())
        }
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

fn apply_resisted_damage_amount(
    world: &mut World,
    target_id: ObjectId,
    damage_type: &str,
    damage: u32,
) -> Result<u32, RejectionReason> {
    let target = entity(target_id)?;
    let multiplier = effect_multiplier(world, target, damage_type)?;
    let damage = ((damage as f64) * multiplier).floor() as u32;
    world
        .resource_mut::<PendingDamage>()
        .push(target, damage, damage_type.to_string());
    Ok(damage)
}

fn heal_drone(world: &mut World, object_id: ObjectId, amount: u32) {
    if amount == 0 {
        return;
    }
    if let Ok(object) = entity(object_id)
        && world.entity(object).get::<Drone>().is_some()
    {
        world.resource_mut::<PendingHeal>().push(object, amount);
    }
}

// ── P2-8 Allied Transfer ──

fn validate_allied_transfer(
    world: &mut World,
    from_player: PlayerId,
    to_player: PlayerId,
    resource: &str,
    amount: u32,
) -> CommandResult {
    if from_player == to_player {
        return Err(RejectionReason::NotFriendly);
    }

    let first_spawn = world.resource::<crate::systems::PlayerFirstSpawnTick>();
    let current_tick = world.resource::<CurrentTick>().0;
    if !first_spawn.0.contains_key(&to_player) {
        return Err(RejectionReason::PlayerNotFound);
    }

    // Check cooldown
    let cooldowns = world.resource::<AlliedTransferCooldowns>();
    if let Some(next_allowed) = cooldowns.0.get(&(from_player, to_player))
        && current_tick < *next_allowed
    {
        return Err(RejectionReason::CooldownActive);
    }

    // Check daily cap
    let daily_usage = world.resource::<AlliedTransferDailyUsage>();
    let used = daily_usage.0.get(&from_player).copied().unwrap_or_default();
    if used.saturating_add(amount) > ALLIED_DAILY_CAP {
        return Err(RejectionReason::DailyTransferCapExceeded);
    }

    // Check sender has enough in global storage
    let available = world
        .resource::<PlayerGlobalStorage>()
        .0
        .get(&from_player)
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

fn apply_allied_transfer(
    world: &mut World,
    from_player: PlayerId,
    to_player: PlayerId,
    resource: &str,
    amount: u32,
) -> CommandResult {
    let fee = amount.saturating_mul(ALLIED_TRANSFER_FEE_BP) / 10_000;
    let deliver_amount = amount.saturating_sub(fee);

    // Deduct from sender's global storage
    subtract_player_resource(
        world
            .resource_mut::<PlayerGlobalStorage>()
            .0
            .entry(from_player)
            .or_default(),
        resource,
        amount,
    );
    record_resource_flow(
        world,
        Some(from_player),
        Some(to_player),
        resource,
        deliver_amount,
        ResourceOperation::AlliedTransfer,
        fee,
        ALLIED_TRANSFER_FEE_BP,
    );

    // Apply cooldown
    let current_tick = world.resource::<CurrentTick>().0;
    world.resource_mut::<AlliedTransferCooldowns>().0.insert(
        (from_player, to_player),
        current_tick + ALLIED_TRANSFER_COOLDOWN,
    );

    // Track daily usage
    world
        .resource_mut::<AlliedTransferDailyUsage>()
        .0
        .entry(from_player)
        .and_modify(|used| *used = used.saturating_add(amount))
        .or_insert(amount);

    // Queue pending delivery
    world
        .resource_mut::<PendingAlliedTransfers>()
        .0
        .push(PendingAlliedTransfer {
            from_player,
            to_player,
            resource: resource.to_string(),
            amount,
            deliver_amount,
            remaining_ticks: ALLIED_TRANSFER_DELAY,
        });

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
    record_resource_flow(
        world,
        Some(player_id),
        Some(player_id),
        resource,
        amount.saturating_sub(transfer_fee(
            amount,
            config.transfer_to_global_fee_per_10_000,
        )),
        ResourceOperation::GlobalDeposit,
        transfer_fee(amount, config.transfer_to_global_fee_per_10_000),
        config.transfer_to_global_fee_per_10_000,
    );
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
    record_resource_flow(
        world,
        Some(player_id),
        Some(player_id),
        resource,
        amount.saturating_sub(transfer_fee(
            amount,
            config.transfer_from_global_fee_per_10_000,
        )),
        ResourceOperation::GlobalWithdraw,
        transfer_fee(amount, config.transfer_from_global_fee_per_10_000),
        config.transfer_from_global_fee_per_10_000,
    );
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

fn ensure_drone_can_take_main_action(
    drone: &Drone,
    tick: Tick,
    part: BodyPart,
    check_fatigue: bool,
) -> CommandResult {
    ensure_drone_can_act(drone, part, check_fatigue)?;
    ensure_drone_main_action_available(drone, tick)
}

fn ensure_drone_main_action_available(drone: &Drone, tick: Tick) -> CommandResult {
    if drone.last_action_tick == tick {
        return Err(RejectionReason::CooldownActive);
    }
    Ok(())
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
            .resource::<PendingEntityCreation>()
            .entries
            .iter()
            .any(|entry| match &entry.kind {
                PendingEntityKind::Drone {
                    position: pending, ..
                }
                | PendingEntityKind::Structure {
                    position: pending, ..
                } => *pending == position,
            })
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

fn refund_recycle_cost(world: &mut World, player_id: PlayerId, refund: &ResourceCost) {
    for (resource, amount) in refund {
        if *amount == 0 {
            continue;
        }
        add_player_local_resource(world, player_id, resource, *amount);
    }
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

fn resource_target_player(world: &mut World, object_id: ObjectId) -> Option<PlayerId> {
    let entity = entity(object_id).ok()?;
    let entity_ref = world.get_entity(entity).ok()?;
    if let Some(drone) = entity_ref.get::<Drone>() {
        return Some(drone.owner);
    }
    if let Some(owner) = entity_ref.get::<Owner>() {
        return Some(owner.0);
    }
    if let Some(structure) = entity_ref.get::<Structure>() {
        return structure.owner;
    }
    if let Some(controller) = entity_ref.get::<Controller>() {
        return controller.owner;
    }
    None
}

fn record_resource_cost(
    world: &mut World,
    player_id: PlayerId,
    cost: &ResourceCost,
    skip_energy: bool,
    operation: ResourceOperation,
) {
    for (resource, amount) in cost {
        if skip_energy && resource == ENERGY_RESOURCE || *amount == 0 {
            continue;
        }
        record_resource_flow(
            world,
            Some(player_id),
            None,
            resource,
            *amount,
            operation,
            0,
            0,
        );
    }
}

fn record_resource_award(
    world: &mut World,
    player_id: PlayerId,
    award: &ResourceCost,
    operation: ResourceOperation,
) {
    for (resource, amount) in award {
        if *amount == 0 {
            continue;
        }
        record_resource_flow(
            world,
            None,
            Some(player_id),
            resource,
            *amount,
            operation,
            0,
            0,
        );
    }
}

#[allow(clippy::too_many_arguments)]
fn record_resource_flow(
    world: &mut World,
    source: Option<PlayerId>,
    target: Option<PlayerId>,
    resource: &str,
    amount: u32,
    operation: ResourceOperation,
    fee_paid: u32,
    basis_points_used: u32,
) {
    if amount == 0 && fee_paid == 0 {
        return;
    }
    let tick = world.resource::<CurrentTick>().0;
    world.resource_mut::<ResourceLedger>().record_attributed(
        tick,
        source,
        target,
        resource,
        i64::from(amount),
        operation,
        fee_paid,
        basis_points_used,
    );
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
    if let Some(mut structure) = world.entity_mut(entity).get_mut::<Structure>()
        && resource == "Energy"
        && let Some(energy) = &mut structure.energy
    {
        *energy += amount;
        return Ok(());
    }
    if world.entity(entity).contains::<Controller>() && resource == "Energy" {
        world
            .resource_mut::<PendingControllerUpgrade>()
            .0
            .push((entity.to_bits(), amount));
        return Ok(());
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
    if let Some(mut structure) = world.entity_mut(entity).get_mut::<Structure>()
        && resource == "Energy"
        && let Some(energy) = &mut structure.energy
    {
        *energy -= amount;
        return Ok(());
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

fn player_fuel_budget(_world: &World, _player_id: PlayerId) -> u64 {
    // Fuel is tracked per tick; return the standard MAX_FUEL as baseline.
    // In practice this consults the tick engine's fuel state.
    MAX_FUEL
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::world::create_world;

    fn admin_provenance() -> AdminCredentialProvenance {
        admin_provenance_with("admin-cert-1", "fingerprint-1")
    }

    fn admin_provenance_with(
        credential_id: &str,
        credential_fingerprint: &str,
    ) -> AdminCredentialProvenance {
        AdminCredentialProvenance::new(
            credential_id,
            credential_fingerprint,
            "admin_cert",
            "admin:root",
            ["swarm:read", ADMIN_SCOPE, "swarm:read"],
        )
    }

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
            auth: CommandAuth::server_injected(CommandSource::TestHarness, player_id, 1, 1),
            sequence: 1,
            action: CommandAction::Action {
                action_type: action_type.to_string(),
                object_id,
                target_id,
                payload: serde_json::Value::Object(serde_json::Map::new()),
            },
        }
    }

    #[test]
    fn concrete_action_wire_flattens_optional_payload_fields() {
        let action: CommandAction = serde_json::from_value(serde_json::json!({
            "type": "Debilitate",
            "object_id": 7,
            "target_id": 9,
            "damage_type": "Kinetic",
            "cooldown": 5
        }))
        .unwrap();

        let CommandAction::Action {
            action_type,
            object_id,
            target_id,
            payload,
        } = action
        else {
            panic!("concrete action must deserialize to the internal Action variant");
        };
        assert_eq!(action_type, "Debilitate");
        assert_eq!(object_id, 7);
        assert_eq!(target_id, Some(9));
        assert_eq!(payload["damage_type"], "Kinetic");
        assert_eq!(payload["cooldown"], 5);
    }

    #[test]
    fn concrete_custom_wire_preserves_arbitrary_flattened_payload() {
        let action: CommandAction = serde_json::from_value(serde_json::json!({
            "type": "Blink",
            "object_id": 7,
            "target_id": 9,
            "range": 3,
            "mode": "phase",
            "metadata": {"shard": 2}
        }))
        .unwrap();

        let CommandAction::Action {
            action_type,
            object_id,
            target_id,
            payload,
        } = action
        else {
            panic!("custom concrete action must deserialize to the internal Action variant");
        };
        assert_eq!(action_type, "Blink");
        assert_eq!(object_id, 7);
        assert_eq!(target_id, Some(9));
        assert_eq!(payload["range"], 3);
        assert_eq!(payload["mode"], "phase");
        assert_eq!(payload["metadata"]["shard"], 2);
    }

    #[test]
    fn exact_core_wire_rejects_cross_variant_fields_and_empty_spawn_body() {
        let move_with_target = serde_json::from_value::<CommandAction>(serde_json::json!({
            "type": "Move",
            "object_id": 7,
            "direction": "Top",
            "target_id": 9
        }))
        .unwrap_err();
        assert!(move_with_target.to_string().contains("target_id"));

        assert!(
            serde_json::from_value::<CommandAction>(serde_json::json!({
                "type": "ClaimController",
                "object_id": 7,
                "controller_id": 9
            }))
            .is_err()
        );

        assert!(
            serde_json::from_value::<CommandAction>(serde_json::json!({
                "type": "Spawn",
                "object_id": 7,
                "spawn_id": 9,
                "body_parts": []
            }))
            .is_err()
        );
    }

    #[test]
    fn admin_command_auth_requires_valid_persisted_provenance() {
        let missing = serde_json::from_value::<CommandAuth>(serde_json::json!({
            "source": "Admin",
            "player_id": 7,
            "tick_submitted": 1,
            "tick_target": 1
        }))
        .unwrap_err();

        assert!(missing.to_string().contains("provenance"));

        let invalid_scope = serde_json::from_value::<CommandAuth>(serde_json::json!({
            "source": "Admin",
            "player_id": 7,
            "tick_submitted": 1,
            "tick_target": 1,
            "admin_credential_provenance": {
                "credential_id": "admin-cert-1",
                "credential_fingerprint": "fingerprint-1",
                "auth_mode": "admin_cert",
                "admin_identity": "admin:root",
                "canonical_scopes": ["swarm:read"]
            }
        }))
        .unwrap_err();
        assert!(invalid_scope.to_string().contains(ADMIN_SCOPE));

        let auth = CommandAuth::server_injected_admin(7, 1, 1, admin_provenance()).unwrap();
        let value = serde_json::to_value(&auth).unwrap();
        assert_eq!(
            value["admin_credential_provenance"]["canonical_scopes"][0],
            "swarm:admin"
        );
        assert_eq!(
            value["admin_credential_provenance"]["canonical_scopes"][1],
            "swarm:read"
        );
        let decoded: CommandAuth = serde_json::from_value(value).unwrap();
        assert!(decoded.has_valid_admin_credential_provenance());

        assert_eq!(
            admin_source_gate(
                7,
                1,
                CommandIntent {
                    sequence: 1,
                    action: CommandAction::Recycle { object_id: 9 },
                },
                admin_provenance(),
            )
            .unwrap()
            .auth,
            CommandAuth::server_injected_admin(7, 1, 1, admin_provenance()).unwrap()
        );

        assert_eq!(
            source_gate(
                7,
                1,
                CommandSource::Admin,
                CommandIntent {
                    sequence: 1,
                    action: CommandAction::Recycle { object_id: 9 },
                },
            ),
            Err(RejectionReason::AuthContextInvalid)
        );
    }

    #[test]
    fn admin_command_auth_round_trip_persists_provenance_across_restart() {
        let auth = CommandAuth::server_injected_admin(7, 1, 1, admin_provenance()).unwrap();
        let bytes = serde_json::to_vec(&auth).unwrap();
        let decoded: CommandAuth = serde_json::from_slice(&bytes).unwrap();

        assert_eq!(decoded, auth);
        let provenance = decoded.admin_credential_provenance().unwrap();
        assert_eq!(provenance.credential_id(), "admin-cert-1");
        assert_eq!(provenance.credential_fingerprint(), "fingerprint-1");
        assert!(decoded.has_valid_admin_credential_provenance());
    }

    #[test]
    fn admin_command_auth_same_envelope_different_credentials_do_not_collide() {
        let first = CommandAuth::server_injected_admin(
            7,
            1,
            1,
            admin_provenance_with("admin-cert-1", "fingerprint-1"),
        )
        .unwrap();
        let second = CommandAuth::server_injected_admin(
            7,
            1,
            1,
            admin_provenance_with("admin-cert-2", "fingerprint-2"),
        )
        .unwrap();

        assert_ne!(first, second);
        assert_eq!(first.source, second.source);
        assert_eq!(first.player_id, second.player_id);
        assert_eq!(first.tick_submitted, second.tick_submitted);
        assert_eq!(first.tick_target, second.tick_target);

        let first_decoded: CommandAuth =
            serde_json::from_value(serde_json::to_value(&first).unwrap()).unwrap();
        let second_decoded: CommandAuth =
            serde_json::from_value(serde_json::to_value(&second).unwrap()).unwrap();
        assert_eq!(
            first_decoded
                .admin_credential_provenance()
                .unwrap()
                .credential_id(),
            "admin-cert-1"
        );
        assert_eq!(
            second_decoded
                .admin_credential_provenance()
                .unwrap()
                .credential_id(),
            "admin-cert-2"
        );
    }

    #[test]
    fn legacy_action_wrapper_and_reserved_payload_keys_are_rejected() {
        let legacy = serde_json::from_value::<CommandAction>(serde_json::json!({
            "type": "Action",
            "action_type": "Attack",
            "object_id": 7,
            "target_id": 9
        }))
        .unwrap_err();
        assert!(legacy.to_string().contains("wire type Action is internal"));

        assert!(
            serde_json::from_value::<CommandAction>(serde_json::json!({
                "type": "Attack",
                "object_id": 7,
                "target_id": 9,
                "payload": {"cooldown": 1}
            }))
            .is_err()
        );

        let action = CommandAction::Action {
            action_type: "Attack".to_string(),
            object_id: 7,
            target_id: Some(9),
            payload: serde_json::json!({"type": "Move"}),
        };
        assert!(serde_json::to_value(action).is_err());

        let action = CommandAction::Action {
            action_type: "Attack".to_string(),
            object_id: 7,
            target_id: Some(9),
            payload: serde_json::json!({"payload": {"cooldown": 1}}),
        };
        assert!(serde_json::to_value(action).is_err());
    }

    fn raw_move(player_id: PlayerId, tick: Tick, sequence: u32, object_id: ObjectId) -> RawCommand {
        source_gate(
            player_id,
            tick,
            CommandSource::Wasm,
            CommandIntent {
                sequence,
                action: CommandAction::Move {
                    object_id,
                    direction: Direction::Top,
                },
            },
        )
        .unwrap()
    }

    fn expected_seeded_player_order(
        mut players: Vec<PlayerId>,
        world_seed: u64,
        tick: Tick,
    ) -> Vec<PlayerId> {
        players.sort_unstable();
        players.dedup();

        let mut hasher = blake3::Hasher::new();
        hasher.update(b"shuffle");
        hasher.update(&world_seed.to_le_bytes());
        hasher.update(&tick.to_le_bytes());
        let mut reader = hasher.finalize_xof();

        for i in 0..players.len() {
            let remaining = players.len() - i;
            let offset = loop {
                let mut bytes = [0_u8; 8];
                reader.fill(&mut bytes);
                if let Some(offset) = unbiased_shuffle_offset(u64::from_le_bytes(bytes), remaining)
                {
                    break offset;
                }
            };
            players.swap(i, i + offset);
        }

        players
    }

    #[test]
    fn seeded_shuffle_rejects_biased_tail_samples() {
        let bound = 3;
        let unbiased_limit = u64::MAX - (u64::MAX % bound as u64);

        assert_eq!(unbiased_shuffle_offset(unbiased_limit - 1, bound), Some(2));
        assert_eq!(unbiased_shuffle_offset(unbiased_limit, bound), None);
        assert_eq!(unbiased_shuffle_offset(u64::MAX, bound), None);
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
    fn sort_raw_commands_uses_seeded_shuffle_before_player_id() {
        let tick = 37;
        let world_seed = 99;
        let mut commands = vec![
            raw_move(1, tick, 0, 101),
            raw_move(2, tick, 0, 201),
            raw_move(3, tick, 0, 301),
            raw_move(4, tick, 0, 401),
            raw_move(5, tick, 0, 501),
        ];
        let expected = expected_seeded_player_order(vec![1, 2, 3, 4, 5], world_seed, tick);
        assert_ne!(
            expected,
            vec![1, 2, 3, 4, 5],
            "fixture must expose player-id sort bias"
        );

        sort_raw_commands_with_seed(&mut commands, world_seed);

        let actual = commands
            .iter()
            .map(|command| command.player_id)
            .collect::<Vec<_>>();
        assert_eq!(actual, expected);
    }

    #[test]
    fn active_player_with_zero_commands_changes_shuffle_slots() {
        let tick = 42;
        let active_players = vec![1, 2, 3, 4];
        let command_players = vec![1, 2, 4];
        let (world_seed, expected, command_derived) = (0_u64..1_000)
            .find_map(|seed| {
                let active_order = expected_seeded_player_order(active_players.clone(), seed, tick);
                let expected = active_order
                    .into_iter()
                    .filter(|player_id| command_players.contains(player_id))
                    .collect::<Vec<_>>();
                let command_derived =
                    expected_seeded_player_order(command_players.clone(), seed, tick);
                (expected != command_derived).then_some((seed, expected, command_derived))
            })
            .expect("fixture must expose zero-command active player slot influence");

        let mut commands = command_players
            .iter()
            .map(|player_id| raw_move(*player_id, tick, 0, u64::from(*player_id) * 100))
            .collect::<Vec<_>>();

        sort_raw_commands_for_active_players(&mut commands, world_seed, &active_players);

        let actual = commands
            .iter()
            .map(|command| command.player_id)
            .collect::<Vec<_>>();
        assert_eq!(actual, expected);
        assert_ne!(actual, command_derived);
    }

    #[test]
    fn admin_uses_standard_pipeline_with_actor_ownership_relaxed() {
        let mut world = create_world();
        let drone = world.spawn_drone(1, 10, 10, vec![BodyPart::Move]);
        let drone_id = object_id(drone);
        let intent = CommandIntent {
            sequence: 1,
            action: CommandAction::Move {
                object_id: drone_id,
                direction: Direction::Top,
            },
        };

        assert_eq!(
            world.submit_intent(99, 1, CommandSource::Wasm, intent.clone()),
            Err(RejectionReason::NotOwner)
        );
        let raw = admin_source_gate(99, 1, intent, admin_provenance()).unwrap();
        world
            .submit_raw_command(raw)
            .expect("admin should follow normal validation/apply with actor ownership relaxed");

        let position = world.app.world().entity(drone).get::<Position>().unwrap();
        assert_eq!((position.x, position.y), (10, 9));
    }

    #[test]
    fn drone_can_only_take_one_main_action_per_tick() {
        let mut world = create_world();
        let drone = world.spawn_drone(1, 10, 10, vec![BodyPart::Move]);
        let object_id = object_id(drone);

        world
            .submit_intent(
                1,
                9,
                CommandSource::Wasm,
                CommandIntent {
                    sequence: 1,
                    action: CommandAction::Move {
                        object_id,
                        direction: Direction::Top,
                    },
                },
            )
            .unwrap();

        assert_eq!(
            world.submit_intent(
                1,
                9,
                CommandSource::Wasm,
                CommandIntent {
                    sequence: 2,
                    action: CommandAction::Move {
                        object_id,
                        direction: Direction::Bottom,
                    },
                },
            ),
            Err(RejectionReason::CooldownActive)
        );
    }

    #[test]
    fn custom_action_active_cooldown_returns_cooldown_active() {
        let mut world = create_world();
        let drone = world.spawn_drone(1, 10, 10, vec![BodyPart::Move]);
        let object_id = object_id(drone);
        world
            .app
            .world_mut()
            .insert_resource(CustomActionRegistry::from_defs(vec![CustomActionDef {
                name: "Blink".to_string(),
                cooldown: Some(5),
                ..Default::default()
            }]));
        world
            .app
            .world_mut()
            .insert_resource(CustomActionCooldowns::default());
        world
            .app
            .world_mut()
            .resource_mut::<CustomActionCooldowns>()
            .0
            .insert((object_id, "Blink".to_string()), 10);

        assert_eq!(
            world.submit_raw_command(raw_custom(1, "Blink", object_id, None)),
            Err(RejectionReason::CooldownActive)
        );
    }

    #[test]
    fn transfer_does_not_consume_drone_main_action_quota() {
        let mut world = create_world();
        let source = world.spawn_drone(1, 10, 10, vec![BodyPart::Move, BodyPart::Carry]);
        let target = world.spawn_drone(1, 10, 11, vec![BodyPart::Carry]);
        let source_id = object_id(source);
        let target_id = object_id(target);

        world
            .app
            .world_mut()
            .entity_mut(source)
            .get_mut::<Drone>()
            .unwrap()
            .carry
            .insert(ENERGY_RESOURCE.to_string(), 5);

        world
            .submit_intent(
                1,
                11,
                CommandSource::Wasm,
                CommandIntent {
                    sequence: 1,
                    action: CommandAction::Transfer {
                        object_id: source_id,
                        target_id,
                        resource: ENERGY_RESOURCE.to_string(),
                        amount: 1,
                    },
                },
            )
            .unwrap();

        assert_eq!(
            world.submit_intent(
                1,
                11,
                CommandSource::Wasm,
                CommandIntent {
                    sequence: 2,
                    action: CommandAction::Move {
                        object_id: source_id,
                        direction: Direction::Top,
                    },
                },
            ),
            Ok(())
        );
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
        world.run_tick_for(1);

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
        world.run_tick_for(1);

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
