use bevy::prelude::*;
use indexmap::IndexMap;
use schemars::JsonSchema;
use serde::de::DeserializeOwned;
use serde::ser::SerializeMap;
use serde::{Deserialize, Deserializer, Serialize, Serializer};
use std::collections::{BTreeMap, BTreeSet, HashSet};
use swarm_engine_api::abi::{
    self, CommandAction as AbiCommandAction, CommandIntent as AbiCommandIntent,
    TickResult as AbiTickResult,
};
use swarm_engine_api::ids::{BodyPart, DamageType, PlayerId, RoomId};
use swarm_engine_plugin_sdk::buffers::{
    PendingDamage, PendingHeal, PendingSpecialAttack, SpecialAttackKind, StatusActionIntent,
};
use swarm_engine_plugin_sdk::components::{
    BodyPartRegistry, Controller, Drone, Owner, Position, Resource, Structure, StructureType,
};
use ts_rs::TS;

use crate::components::*;
use crate::onboarding::{OnboardingEvent, send_onboarding_event};
use crate::resource_ledger::{
    LedgerAccount, PluginSettlementKind, ResourceLedger, ResourceOperation,
};
use crate::resources::{
    ALLIED_DAILY_CAP, ALLIED_TRANSFER_COOLDOWN, ALLIED_TRANSFER_DELAY, ALLIED_TRANSFER_FEE_BP,
    AlliedTransferCooldowns, AlliedTransferDailyUsage, AuctionSettlement, ContractSettlement,
    CurrentTick, EscrowSettlement, GlobalStorageConfig, GlobalTransferDirection, LendingSettlement,
    MerchantQuote, P2POffer, PendingAlliedTransfer, PendingAlliedTransfers, PendingGlobalTransfer,
    PendingGlobalTransfers, PlayerGlobalStorage, PlayerLocalStorage, ResourceCost,
    ResourceRegistry, SettlementId, SettlementIdNonce, SettlementKey, SettlementKind,
    SettlementPhase, SettlementState, SettlementStatus,
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
const OVERLOAD_FUEL_FLOOR: u64 = MAX_FUEL / 5;
const ADMIN_SCOPE: &str = "swarm:admin";

#[derive(Resource, Debug, Clone, Default)]
pub struct CustomActionCooldowns(pub(crate) IndexMap<(ObjectId, String), Tick>);

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, JsonSchema, TS)]
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

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, JsonSchema, TS)]
pub enum Direction {
    Top,
    TopRight,
    BottomRight,
    Bottom,
    BottomLeft,
    TopLeft,
}

#[derive(Debug, Clone, PartialEq, Eq, JsonSchema, TS)]
#[schemars(tag = "type")]
#[schemars(deny_unknown_fields)]
#[ts(tag = "type")]
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
    #[schemars(skip)]
    #[ts(skip)]
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
        #[schemars(with = "String")]
        #[ts(type = "string")]
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

pub const ECONOMY_COMMAND_ACTIONS: &[&str] = &[
    "CreateContractSettlement",
    "SettleContract",
    "CancelContract",
    "CreateMerchantQuote",
    "AcceptMerchantTrade",
    "CreateP2POffer",
    "AcceptP2POffer",
    "CancelP2POffer",
    "RefundP2POffer",
    "CreateAuction",
    "BidAuction",
    "SettleAuction",
    "CancelAuction",
    "CreateEscrow",
    "ReleaseEscrow",
    "RefundEscrow",
    "CreateLoanOffer",
    "AcceptLoan",
    "RepayLoan",
    "DefaultLoan",
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

fn is_core_or_economy_action(action_type: &str) -> bool {
    CORE_COMMAND_ACTIONS.contains(&action_type) || ECONOMY_COMMAND_ACTIONS.contains(&action_type)
}

fn is_core_action(action_type: &str) -> bool {
    CORE_COMMAND_ACTIONS.contains(&action_type)
}

fn is_reserved_wire_action(action_type: &str) -> bool {
    is_core_action(action_type)
        || SPECIAL_COMMAND_ACTIONS.contains(&action_type)
        || action_type == "Action"
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CommandSchemaField {
    pub name: &'static str,
    pub type_name: &'static str,
    pub required: bool,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct CommandSchemaMetadata {
    pub settlement_kind: Option<&'static str>,
    pub settlement_phase: Option<&'static str>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CommandSchemaBranch {
    pub name: &'static str,
    pub fields: Vec<CommandSchemaField>,
    pub metadata: CommandSchemaMetadata,
    pub custom_wildcard: bool,
}

const fn schema_field(name: &'static str, type_name: &'static str) -> CommandSchemaField {
    CommandSchemaField {
        name,
        type_name,
        required: true,
    }
}

const fn optional_schema_field(name: &'static str, type_name: &'static str) -> CommandSchemaField {
    CommandSchemaField {
        name,
        type_name,
        required: false,
    }
}

fn schema_branch(name: &'static str, fields: &[CommandSchemaField]) -> CommandSchemaBranch {
    CommandSchemaBranch {
        name,
        fields: fields.to_vec(),
        metadata: CommandSchemaMetadata::default(),
        custom_wildcard: false,
    }
}

fn settlement_schema_branch(
    name: &'static str,
    kind: &'static str,
    phase: &'static str,
    fields: &[CommandSchemaField],
) -> CommandSchemaBranch {
    CommandSchemaBranch {
        name,
        fields: fields.to_vec(),
        metadata: CommandSchemaMetadata {
            settlement_kind: Some(kind),
            settlement_phase: Some(phase),
        },
        custom_wildcard: false,
    }
}

pub fn core_command_schema_branches() -> Vec<CommandSchemaBranch> {
    vec![
        schema_branch(
            "Move",
            &[
                schema_field("object_id", "ObjectId"),
                schema_field("direction", "Direction"),
            ],
        ),
        schema_branch(
            "Harvest",
            &[
                schema_field("object_id", "ObjectId"),
                schema_field("target_id", "ObjectId"),
                optional_schema_field("resource", "ResourceName"),
            ],
        ),
        schema_branch(
            "Transfer",
            &[
                schema_field("object_id", "ObjectId"),
                schema_field("target_id", "ObjectId"),
                schema_field("resource", "ResourceName"),
                schema_field("amount", "ResourceAmount"),
            ],
        ),
        schema_branch(
            "Withdraw",
            &[
                schema_field("object_id", "ObjectId"),
                schema_field("target_id", "ObjectId"),
                schema_field("resource", "ResourceName"),
                schema_field("amount", "ResourceAmount"),
            ],
        ),
        schema_branch(
            "ClaimController",
            &[
                schema_field("object_id", "ObjectId"),
                schema_field("target_id", "ObjectId"),
            ],
        ),
        schema_branch(
            "Spawn",
            &[
                schema_field("object_id", "ObjectId"),
                schema_field("spawn_id", "ObjectId"),
                schema_field("body_parts", "BodyPart[]"),
            ],
        ),
        schema_branch("Recycle", &[schema_field("object_id", "ObjectId")]),
        schema_branch(
            "Build",
            &[
                schema_field("object_id", "ObjectId"),
                schema_field("x", "i32"),
                schema_field("y", "i32"),
                schema_field("structure", "StructureType"),
            ],
        ),
        schema_branch(
            "Repair",
            &[
                schema_field("object_id", "ObjectId"),
                schema_field("target_id", "ObjectId"),
            ],
        ),
        schema_branch(
            "UpgradeController",
            &[
                schema_field("object_id", "ObjectId"),
                schema_field("target_id", "ObjectId"),
            ],
        ),
        schema_branch(
            "TransferToGlobal",
            &[
                schema_field("resource", "ResourceName"),
                schema_field("amount", "ResourceAmount"),
            ],
        ),
        schema_branch(
            "TransferFromGlobal",
            &[
                schema_field("resource", "ResourceName"),
                schema_field("amount", "ResourceAmount"),
            ],
        ),
        schema_branch(
            "AlliedTransfer",
            &[
                schema_field("target_player", "PlayerId"),
                schema_field("resource", "ResourceName"),
                schema_field("amount", "ResourceAmount"),
            ],
        ),
    ]
}

pub fn economy_command_schema_branches() -> Vec<CommandSchemaBranch> {
    vec![
        settlement_schema_branch(
            "CreateContractSettlement",
            "Contract",
            "Reserve",
            &[
                schema_field("settlement_id", "u64"),
                schema_field("nonce", "u64"),
                schema_field("input_resource", "ResourceName"),
                schema_field("input_amount", "ResourceAmount"),
                schema_field("output_resource", "ResourceName"),
                schema_field("output_amount", "ResourceAmount"),
                optional_schema_field("counterparty", "PlayerId"),
                optional_schema_field("expires_at", "u64"),
            ],
        ),
        settlement_schema_branch(
            "SettleContract",
            "Contract",
            "Settle",
            &[schema_field("settlement_id", "u64")],
        ),
        settlement_schema_branch(
            "CancelContract",
            "Contract",
            "Cancel",
            &[schema_field("settlement_id", "u64")],
        ),
        settlement_schema_branch(
            "CreateMerchantQuote",
            "MerchantTrade",
            "Reserve",
            &[
                schema_field("quote_id", "u64"),
                schema_field("player_id", "PlayerId"),
                schema_field("pay_resource", "ResourceName"),
                schema_field("pay_amount", "ResourceAmount"),
                schema_field("receive_resource", "ResourceName"),
                schema_field("receive_amount", "ResourceAmount"),
                schema_field("expires_at", "u64"),
            ],
        ),
        settlement_schema_branch(
            "AcceptMerchantTrade",
            "MerchantTrade",
            "Settle",
            &[
                schema_field("quote_id", "u64"),
                schema_field("min_receive", "ResourceAmount"),
            ],
        ),
        settlement_schema_branch(
            "CreateP2POffer",
            "P2POffer",
            "Reserve",
            &[
                schema_field("offer_id", "u64"),
                schema_field("nonce", "u64"),
                schema_field("give_resource", "ResourceName"),
                schema_field("give_amount", "ResourceAmount"),
                schema_field("want_resource", "ResourceName"),
                schema_field("want_amount", "ResourceAmount"),
                schema_field("expires_at", "u64"),
            ],
        ),
        settlement_schema_branch(
            "AcceptP2POffer",
            "P2POffer",
            "Accept",
            &[schema_field("offer_id", "u64")],
        ),
        settlement_schema_branch(
            "CancelP2POffer",
            "P2POffer",
            "Cancel",
            &[schema_field("offer_id", "u64")],
        ),
        settlement_schema_branch(
            "RefundP2POffer",
            "P2POffer",
            "Refund",
            &[schema_field("offer_id", "u64")],
        ),
        settlement_schema_branch(
            "CreateAuction",
            "Auction",
            "Reserve",
            &[
                schema_field("auction_id", "u64"),
                schema_field("nonce", "u64"),
                schema_field("lot_resource", "ResourceName"),
                schema_field("lot_amount", "ResourceAmount"),
                schema_field("bid_resource", "ResourceName"),
                schema_field("min_bid", "ResourceAmount"),
                schema_field("ends_at", "u64"),
            ],
        ),
        settlement_schema_branch(
            "BidAuction",
            "Auction",
            "Accept",
            &[
                schema_field("auction_id", "u64"),
                schema_field("bid_amount", "ResourceAmount"),
            ],
        ),
        settlement_schema_branch(
            "SettleAuction",
            "Auction",
            "Settle",
            &[schema_field("auction_id", "u64")],
        ),
        settlement_schema_branch(
            "CancelAuction",
            "Auction",
            "Cancel",
            &[schema_field("auction_id", "u64")],
        ),
        settlement_schema_branch(
            "CreateEscrow",
            "Escrow",
            "Reserve",
            &[
                schema_field("escrow_id", "u64"),
                schema_field("nonce", "u64"),
                schema_field("payee", "PlayerId"),
                schema_field("arbiter", "PlayerId"),
                schema_field("resource", "ResourceName"),
                schema_field("amount", "ResourceAmount"),
            ],
        ),
        settlement_schema_branch(
            "ReleaseEscrow",
            "Escrow",
            "Settle",
            &[schema_field("escrow_id", "u64")],
        ),
        settlement_schema_branch(
            "RefundEscrow",
            "Escrow",
            "Refund",
            &[schema_field("escrow_id", "u64")],
        ),
        settlement_schema_branch(
            "CreateLoanOffer",
            "Lending",
            "Reserve",
            &[
                schema_field("loan_id", "u64"),
                schema_field("nonce", "u64"),
                schema_field("borrower", "PlayerId"),
                schema_field("resource", "ResourceName"),
                schema_field("principal", "ResourceAmount"),
                schema_field("repay_amount", "ResourceAmount"),
                schema_field("due_at", "u64"),
            ],
        ),
        settlement_schema_branch(
            "AcceptLoan",
            "Lending",
            "Accept",
            &[schema_field("loan_id", "u64")],
        ),
        settlement_schema_branch(
            "RepayLoan",
            "Lending",
            "Repay",
            &[schema_field("loan_id", "u64")],
        ),
        settlement_schema_branch(
            "DefaultLoan",
            "Lending",
            "Default",
            &[schema_field("loan_id", "u64")],
        ),
    ]
}

pub fn command_schema_branches() -> Vec<CommandSchemaBranch> {
    let mut branches = core_command_schema_branches();
    for name in SPECIAL_COMMAND_ACTIONS {
        branches.push(schema_branch(
            name,
            &[
                schema_field("object_id", "ObjectId"),
                schema_field("target_id", "ObjectId"),
                optional_schema_field("resource", "ResourceName"),
                optional_schema_field("amount", "ResourceAmount"),
                optional_schema_field("range", "u32"),
                optional_schema_field("structure", "StructureType"),
                optional_schema_field("damage_type", "DamageType"),
                optional_schema_field("cooldown", "u32"),
            ],
        ));
    }
    branches.push(CommandSchemaBranch {
        name: "CustomAction",
        fields: vec![
            schema_field("object_id", "ObjectId"),
            optional_schema_field("target_id", "ObjectId"),
            optional_schema_field("resource", "ResourceName"),
            optional_schema_field("amount", "ResourceAmount"),
            optional_schema_field("range", "u32"),
            optional_schema_field("structure", "StructureType"),
            optional_schema_field("damage_type", "DamageType"),
            optional_schema_field("cooldown", "u32"),
        ],
        metadata: CommandSchemaMetadata::default(),
        custom_wildcard: true,
    });
    branches
}

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
            "Action" => legacy_action_wrapper(fields)?,
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
            "CreateContractSettlement" => economy_action_from_fields(
                "CreateContractSettlement",
                fields,
                CREATE_CONTRACT_ACTION_FIELDS,
            )?,
            "SettleContract" => {
                economy_action_from_fields("SettleContract", fields, SETTLEMENT_ID_ACTION_FIELDS)?
            }
            "CancelContract" => {
                economy_action_from_fields("CancelContract", fields, SETTLEMENT_ID_ACTION_FIELDS)?
            }
            "CreateMerchantQuote" => economy_action_from_fields(
                "CreateMerchantQuote",
                fields,
                CREATE_MERCHANT_QUOTE_ACTION_FIELDS,
            )?,
            "AcceptMerchantTrade" => economy_action_from_fields(
                "AcceptMerchantTrade",
                fields,
                MERCHANT_ACCEPT_ACTION_FIELDS,
            )?,
            "CreateP2POffer" => economy_action_from_fields(
                "CreateP2POffer",
                fields,
                CREATE_P2P_OFFER_ACTION_FIELDS,
            )?,
            "AcceptP2POffer" => {
                economy_action_from_fields("AcceptP2POffer", fields, OFFER_ID_ACTION_FIELDS)?
            }
            "CancelP2POffer" => {
                economy_action_from_fields("CancelP2POffer", fields, OFFER_ID_ACTION_FIELDS)?
            }
            "RefundP2POffer" => {
                economy_action_from_fields("RefundP2POffer", fields, OFFER_ID_ACTION_FIELDS)?
            }
            "CreateAuction" => {
                economy_action_from_fields("CreateAuction", fields, CREATE_AUCTION_ACTION_FIELDS)?
            }
            "BidAuction" => {
                economy_action_from_fields("BidAuction", fields, BID_AUCTION_ACTION_FIELDS)?
            }
            "SettleAuction" => {
                economy_action_from_fields("SettleAuction", fields, AUCTION_ID_ACTION_FIELDS)?
            }
            "CancelAuction" => {
                economy_action_from_fields("CancelAuction", fields, AUCTION_ID_ACTION_FIELDS)?
            }
            "CreateEscrow" => {
                economy_action_from_fields("CreateEscrow", fields, CREATE_ESCROW_ACTION_FIELDS)?
            }
            "ReleaseEscrow" => {
                economy_action_from_fields("ReleaseEscrow", fields, ESCROW_ID_ACTION_FIELDS)?
            }
            "RefundEscrow" => {
                economy_action_from_fields("RefundEscrow", fields, ESCROW_ID_ACTION_FIELDS)?
            }
            "CreateLoanOffer" => {
                economy_action_from_fields("CreateLoanOffer", fields, CREATE_LOAN_ACTION_FIELDS)?
            }
            "AcceptLoan" => {
                economy_action_from_fields("AcceptLoan", fields, LOAN_ID_ACTION_FIELDS)?
            }
            "RepayLoan" => economy_action_from_fields("RepayLoan", fields, LOAN_ID_ACTION_FIELDS)?,
            "DefaultLoan" => {
                economy_action_from_fields("DefaultLoan", fields, LOAN_ID_ACTION_FIELDS)?
            }
            action if SPECIAL_COMMAND_ACTIONS.contains(&action) => {
                special_action_from_fields(action, fields)?
            }
            custom => Self::Action {
                action_type: custom.to_string(),
                object_id: required_action_field(fields, "object_id")?,
                target_id: optional_action_field(fields, "target_id")?,
                payload: command_action_payload(fields)?,
            },
        })
    }
}

fn legacy_action_wrapper<E>(
    fields: &serde_json::Map<String, serde_json::Value>,
) -> Result<CommandAction, E>
where
    E: serde::de::Error,
{
    const LEGACY_ACTION_FIELDS: &[&str] =
        &["type", "action_type", "object_id", "target_id", "payload"];
    ensure_exact_action_fields(fields, LEGACY_ACTION_FIELDS)?;
    let action_type: String = required_action_field(fields, "action_type")?;
    if is_core_or_economy_action(&action_type) || action_type == "Action" {
        return Err(E::custom(format!(
            "legacy Action wrapper uses reserved non-special action_type {action_type}"
        )));
    }
    if !SPECIAL_COMMAND_ACTIONS.contains(&action_type.as_str()) {
        return Err(E::custom(format!(
            "legacy Action wrapper uses unknown action_type {action_type}"
        )));
    }
    let object_id = required_action_field(fields, "object_id")?;
    let target_id = required_action_field(fields, "target_id")?;
    let special_fields =
        SpecialActionFields::from_legacy_payload(object_id, target_id, fields.get("payload"))?;
    special_action_from_name(&action_type, special_fields)
}

fn economy_action_from_fields<E>(
    action_type: &str,
    fields: &serde_json::Map<String, serde_json::Value>,
    allowed_fields: &'static [&'static str],
) -> Result<CommandAction, E>
where
    E: serde::de::Error,
{
    ensure_exact_action_fields(fields, allowed_fields)?;
    let mut payload = serde_json::Map::new();
    for (key, value) in fields {
        if key != "type" {
            payload.insert(key.clone(), value.clone());
        }
    }
    Ok(CommandAction::Action {
        action_type: action_type.to_string(),
        object_id: 0,
        target_id: None,
        payload: serde_json::Value::Object(payload),
    })
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
const SETTLEMENT_ID_ACTION_FIELDS: &[&str] = &["type", "settlement_id"];
const OFFER_ID_ACTION_FIELDS: &[&str] = &["type", "offer_id"];
const AUCTION_ID_ACTION_FIELDS: &[&str] = &["type", "auction_id"];
const ESCROW_ID_ACTION_FIELDS: &[&str] = &["type", "escrow_id"];
const LOAN_ID_ACTION_FIELDS: &[&str] = &["type", "loan_id"];
const CREATE_CONTRACT_ACTION_FIELDS: &[&str] = &[
    "type",
    "settlement_id",
    "nonce",
    "input_resource",
    "input_amount",
    "output_resource",
    "output_amount",
    "counterparty",
    "expires_at",
];
const CREATE_MERCHANT_QUOTE_ACTION_FIELDS: &[&str] = &[
    "type",
    "quote_id",
    "player_id",
    "pay_resource",
    "pay_amount",
    "receive_resource",
    "receive_amount",
    "expires_at",
];
const MERCHANT_ACCEPT_ACTION_FIELDS: &[&str] = &["type", "quote_id", "min_receive"];
const CREATE_P2P_OFFER_ACTION_FIELDS: &[&str] = &[
    "type",
    "offer_id",
    "nonce",
    "give_resource",
    "give_amount",
    "want_resource",
    "want_amount",
    "expires_at",
];
const CREATE_AUCTION_ACTION_FIELDS: &[&str] = &[
    "type",
    "auction_id",
    "nonce",
    "lot_resource",
    "lot_amount",
    "bid_resource",
    "min_bid",
    "ends_at",
];
const BID_AUCTION_ACTION_FIELDS: &[&str] = &["type", "auction_id", "bid_amount"];
const CREATE_ESCROW_ACTION_FIELDS: &[&str] = &[
    "type",
    "escrow_id",
    "nonce",
    "payee",
    "arbiter",
    "resource",
    "amount",
];
const CREATE_LOAN_ACTION_FIELDS: &[&str] = &[
    "type",
    "loan_id",
    "nonce",
    "borrower",
    "resource",
    "principal",
    "repay_amount",
    "due_at",
];
const SPECIAL_ACTION_FIELDS: &[&str] = &[
    "type",
    "object_id",
    "target_id",
    "resource",
    "amount",
    "range",
    "structure",
    "damage_type",
    "cooldown",
];
const CONCRETE_ACTION_RESERVED_FIELDS: &[&str] = &["type", "object_id", "target_id"];
const ACTION_PAYLOAD_RESERVED_FIELDS: &[&str] = &[
    "type",
    "object_id",
    "target_id",
    "payload",
    "action_type",
    "action_name",
];

#[derive(Debug, Clone, PartialEq, Eq)]
struct SpecialActionFields {
    object_id: ObjectId,
    target_id: ObjectId,
    resource: Option<String>,
    amount: Option<u32>,
    range: Option<u32>,
    structure: Option<StructureType>,
    damage_type: Option<String>,
    cooldown: Option<u32>,
}

impl SpecialActionFields {
    fn from_flat_fields<E>(fields: &serde_json::Map<String, serde_json::Value>) -> Result<Self, E>
    where
        E: serde::de::Error,
    {
        ensure_exact_action_fields(fields, SPECIAL_ACTION_FIELDS)?;
        Self::from_payload_fields(
            required_action_field(fields, "object_id")?,
            required_action_field(fields, "target_id")?,
            fields,
        )
    }

    fn from_legacy_payload<E>(
        object_id: ObjectId,
        target_id: ObjectId,
        payload: Option<&serde_json::Value>,
    ) -> Result<Self, E>
    where
        E: serde::de::Error,
    {
        let empty = serde_json::Map::new();
        let payload_fields = match payload {
            None | Some(serde_json::Value::Null) => &empty,
            Some(serde_json::Value::Object(fields)) => fields,
            Some(_) => return Err(E::custom("legacy Action payload must be an object")),
        };
        ensure_special_payload_fields(payload_fields)?;
        Self::from_payload_fields(object_id, target_id, payload_fields)
    }

    fn from_payload_fields<E>(
        object_id: ObjectId,
        target_id: ObjectId,
        fields: &serde_json::Map<String, serde_json::Value>,
    ) -> Result<Self, E>
    where
        E: serde::de::Error,
    {
        Ok(Self {
            object_id,
            target_id,
            resource: optional_action_field(fields, "resource")?,
            amount: optional_action_field(fields, "amount")?,
            range: optional_action_field(fields, "range")?,
            structure: optional_action_field(fields, "structure")?,
            damage_type: optional_action_field(fields, "damage_type")?,
            cooldown: optional_action_field(fields, "cooldown")?,
        })
    }
}

#[derive(Debug, Clone, Copy)]
struct SpecialActionInput<'a> {
    action_type: &'static str,
    object_id: ObjectId,
    target_id: ObjectId,
    resource: Option<&'a str>,
    amount: Option<u32>,
    range: Option<u32>,
    structure: Option<StructureType>,
    damage_type: Option<&'a str>,
    cooldown: Option<u32>,
}

fn special_action_from_fields<E>(
    action_type: &str,
    fields: &serde_json::Map<String, serde_json::Value>,
) -> Result<CommandAction, E>
where
    E: serde::de::Error,
{
    special_action_from_name(action_type, SpecialActionFields::from_flat_fields(fields)?)
}

fn special_action_from_name<E>(
    action_type: &str,
    fields: SpecialActionFields,
) -> Result<CommandAction, E>
where
    E: serde::de::Error,
{
    Ok(match action_type {
        action if SPECIAL_COMMAND_ACTIONS.contains(&action) => CommandAction::Action {
            action_type: action.to_string(),
            object_id: fields.object_id,
            target_id: Some(fields.target_id),
            payload: special_fields_payload(&fields),
        },
        unknown => {
            return Err(E::custom(format!(
                "unknown special command action {unknown}"
            )));
        }
    })
}

fn special_fields_payload(fields: &SpecialActionFields) -> serde_json::Value {
    let mut payload = serde_json::Map::new();
    if let Some(resource) = &fields.resource {
        payload.insert(
            "resource".to_string(),
            serde_json::Value::String(resource.clone()),
        );
    }
    if let Some(amount) = fields.amount {
        payload.insert("amount".to_string(), serde_json::json!(amount));
    }
    if let Some(range) = fields.range {
        payload.insert("range".to_string(), serde_json::json!(range));
    }
    if let Some(structure) = fields.structure {
        payload.insert("structure".to_string(), serde_json::json!(structure));
    }
    if let Some(damage_type) = &fields.damage_type {
        payload.insert(
            "damage_type".to_string(),
            serde_json::Value::String(damage_type.clone()),
        );
    }
    if let Some(cooldown) = fields.cooldown {
        payload.insert("cooldown".to_string(), serde_json::json!(cooldown));
    }
    serde_json::Value::Object(payload)
}

fn ensure_special_payload_fields<E>(
    fields: &serde_json::Map<String, serde_json::Value>,
) -> Result<(), E>
where
    E: serde::de::Error,
{
    for key in fields.keys() {
        if ACTION_PAYLOAD_RESERVED_FIELDS.contains(&key.as_str()) {
            return Err(E::custom(format!(
                "legacy Action payload must not contain reserved field {key}"
            )));
        }
    }
    Ok(())
}

impl CommandAction {
    fn special_action(&self) -> Option<SpecialActionInput<'_>> {
        let Self::Action {
            action_type,
            object_id,
            target_id: Some(target_id),
            payload,
        } = self
        else {
            return None;
        };
        let action_type = SPECIAL_COMMAND_ACTIONS
            .iter()
            .copied()
            .find(|name| *name == action_type)?;
        Some(SpecialActionInput {
            action_type,
            object_id: *object_id,
            target_id: *target_id,
            resource: payload.get("resource").and_then(serde_json::Value::as_str),
            amount: payload
                .get("amount")
                .and_then(serde_json::Value::as_u64)
                .and_then(|value| u32::try_from(value).ok()),
            range: payload_range(payload),
            structure: payload
                .get("structure")
                .cloned()
                .and_then(|value| serde_json::from_value(value).ok()),
            damage_type: payload
                .get("damage_type")
                .and_then(serde_json::Value::as_str),
            cooldown: payload
                .get("cooldown")
                .and_then(serde_json::Value::as_u64)
                .and_then(|value| u32::try_from(value).ok()),
        })
    }

    fn wire_type(&self) -> &str {
        if let Some(special) = self.special_action() {
            return special.action_type;
        }
        match self {
            Self::Move { .. } => "Move",
            Self::Harvest { .. } => "Harvest",
            Self::Transfer { .. } => "Transfer",
            Self::Withdraw { .. } => "Withdraw",
            Self::Action { action_type, .. } => action_type,
            Self::ClaimController { .. } => "ClaimController",
            Self::Spawn { .. } => "Spawn",
            Self::Recycle { .. } => "Recycle",
            Self::Build { .. } => "Build",
            Self::Repair { .. } => "Repair",
            Self::UpgradeController { .. } => "UpgradeController",
            Self::TransferToGlobal { .. } => "TransferToGlobal",
            Self::TransferFromGlobal { .. } => "TransferFromGlobal",
            Self::AlliedTransfer { .. } => "AlliedTransfer",
        }
    }

    fn actor_object_id(&self) -> Option<ObjectId> {
        if let Some(special) = self.special_action() {
            return Some(special.object_id);
        }
        match self {
            Self::Move { object_id, .. }
            | Self::Harvest { object_id, .. }
            | Self::Transfer { object_id, .. }
            | Self::Withdraw { object_id, .. }
            | Self::Action { object_id, .. }
            | Self::ClaimController { object_id, .. }
            | Self::Spawn { object_id, .. }
            | Self::Recycle { object_id }
            | Self::Build { object_id, .. }
            | Self::Repair { object_id, .. }
            | Self::UpgradeController { object_id, .. } => Some(*object_id),
            Self::TransferToGlobal { .. }
            | Self::TransferFromGlobal { .. }
            | Self::AlliedTransfer { .. } => None,
        }
    }
}

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
        if let Some(special) = self.special_action() {
            serialize_special_action(&mut map, special)?;
            return map.end();
        }
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
                if is_reserved_wire_action(&action_type) {
                    return Err(serde::ser::Error::custom(format!(
                        "generic Action cannot serialize reserved action_type {action_type}"
                    )));
                }
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

fn serialize_special_action<S>(map: &mut S, special: SpecialActionInput<'_>) -> Result<(), S::Error>
where
    S: SerializeMap,
{
    map.serialize_entry("type", special.action_type)?;
    map.serialize_entry("object_id", &special.object_id)?;
    map.serialize_entry("target_id", &special.target_id)?;
    if let Some(resource) = special.resource {
        map.serialize_entry("resource", resource)?;
    }
    if let Some(amount) = special.amount {
        map.serialize_entry("amount", &amount)?;
    }
    if let Some(range) = special.range {
        map.serialize_entry("range", &range)?;
    }
    if let Some(structure) = special.structure {
        map.serialize_entry("structure", &structure)?;
    }
    if let Some(damage_type) = special.damage_type {
        map.serialize_entry("damage_type", damage_type)?;
    }
    if let Some(cooldown) = special.cooldown {
        map.serialize_entry("cooldown", &cooldown)?;
    }
    Ok(())
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
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema, TS)]
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
    pub idempotency_key: String,
    pub client_trace_id: Option<String>,
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
    idempotency_key: String,
    #[serde(default)]
    client_trace_id: Option<String>,
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

        let mut map = serializer.serialize_map(Some(if provenance.is_some() { 7 } else { 6 }))?;
        map.serialize_entry("source", &self.source)?;
        map.serialize_entry("player_id", &self.player_id)?;
        map.serialize_entry("tick_submitted", &self.tick_submitted)?;
        map.serialize_entry("tick_target", &self.tick_target)?;
        map.serialize_entry("idempotency_key", &self.idempotency_key)?;
        if let Some(client_trace_id) = &self.client_trace_id {
            map.serialize_entry("client_trace_id", client_trace_id)?;
        }
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
            idempotency_key: if wire.idempotency_key.is_empty() {
                synthetic_idempotency_key(wire.source, wire.player_id, wire.tick_target, 0)
            } else {
                wire.idempotency_key
            },
            client_trace_id: wire.client_trace_id,
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

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum IdempotencyDecision {
    Execute,
    Duplicate,
    Conflict,
}

#[derive(Debug, Default)]
pub(crate) struct CommandIdempotencyTable {
    entries: BTreeMap<(PlayerId, Tick, String), [u8; 32]>,
}

impl CommandIdempotencyTable {
    pub(crate) fn check(&mut self, command: &RawCommand) -> IdempotencyDecision {
        let idempotency_key = if command.auth.idempotency_key.starts_with("server:") {
            format!("{}:{}", command.auth.idempotency_key, command.sequence)
        } else {
            command.auth.idempotency_key.clone()
        };
        let key = (command.player_id, command.tick, idempotency_key);
        let hash = canonical_command_hash(command);
        match self.entries.get(&key) {
            Some(existing) if *existing == hash => IdempotencyDecision::Duplicate,
            Some(_) => IdempotencyDecision::Conflict,
            None => {
                self.entries.insert(key, hash);
                IdempotencyDecision::Execute
            }
        }
    }
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
    AlreadyActed,
    InvalidBodyPart,
    NotEnoughBodyParts,
    InvalidDirection,
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
    OutOfRange {
        distance: u32,
        max: u32,
    },
    CapacityExceeded,
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
    "CapacityExceeded",
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
        auth: CommandAuth::server_injected_with_intent_metadata(
            source,
            player_id,
            tick,
            tick,
            synthetic_idempotency_key(source, player_id, tick, intent.sequence),
            None,
        ),
        sequence: intent.sequence,
        action: normalize_legacy_action(intent.action),
    })
}

pub fn source_gate_abi(
    player_id: PlayerId,
    tick: Tick,
    intent: AbiCommandIntent,
) -> Result<RawCommand, RejectionReason> {
    if intent.idempotency_key.trim().is_empty() {
        return Err(RejectionReason::SchemaViolation);
    }
    Ok(RawCommand {
        player_id,
        tick,
        source: CommandSource::Wasm,
        auth: CommandAuth::server_injected_with_intent_metadata(
            CommandSource::Wasm,
            player_id,
            tick,
            tick,
            intent.idempotency_key,
            intent.client_trace_id,
        ),
        sequence: intent.sequence,
        action: command_action_from_abi(intent.action)?,
    })
}

pub fn collect_abi_tick_result(
    player_id: PlayerId,
    tick: Tick,
    result: AbiTickResult,
) -> Result<Vec<RawCommand>, TickValidationError> {
    if result.commands.len() > abi::MAX_COMMANDS_PER_PLAYER {
        return Err(TickValidationError::TooManyCommands);
    }
    result
        .commands
        .into_iter()
        .map(|intent| {
            source_gate_abi(player_id, tick, intent)
                .map_err(|_| TickValidationError::SchemaViolation)
        })
        .collect()
}

pub fn collect_wasm_tick_result_bytes(
    player_id: PlayerId,
    tick: Tick,
    bytes: &[u8],
) -> Result<Vec<RawCommand>, TickValidationError> {
    let result =
        abi::decode_tick_result(bytes).map_err(|_| TickValidationError::SchemaViolation)?;
    collect_abi_tick_result(player_id, tick, result)
}

fn command_action_from_abi(action: AbiCommandAction) -> Result<CommandAction, RejectionReason> {
    Ok(match action {
        AbiCommandAction::Move {
            object_id,
            direction,
        } => CommandAction::Move {
            object_id,
            direction: direction_from_abi(direction),
        },
        AbiCommandAction::Harvest {
            object_id,
            target_id,
            resource,
        } => CommandAction::Harvest {
            object_id,
            target_id,
            resource: resource.map(|resource| resource.as_str().to_string()),
        },
        AbiCommandAction::Transfer {
            object_id,
            target_id,
            resource,
            amount,
        } => CommandAction::Transfer {
            object_id,
            target_id,
            resource: resource.as_str().to_string(),
            amount,
        },
        AbiCommandAction::Withdraw {
            object_id,
            target_id,
            resource,
            amount,
        } => CommandAction::Withdraw {
            object_id,
            target_id,
            resource: resource.as_str().to_string(),
            amount,
        },
        AbiCommandAction::Action {
            action_type,
            object_id,
            payload,
        } => {
            let json_payload = action_payload_to_json(payload);
            let target_id = json_payload
                .get("target_id")
                .and_then(serde_json::Value::as_u64);
            CommandAction::Action {
                action_type,
                object_id,
                target_id,
                payload: json_payload,
            }
        }
        AbiCommandAction::ClaimController {
            object_id,
            target_id,
        } => CommandAction::ClaimController {
            object_id,
            target_id,
        },
        AbiCommandAction::Spawn {
            object_id,
            spawn_id,
            body_parts,
        } => CommandAction::Spawn {
            object_id,
            spawn_id,
            body_parts,
        },
        AbiCommandAction::Recycle { object_id } => CommandAction::Recycle { object_id },
        AbiCommandAction::Build {
            object_id,
            structure,
            x,
            y,
        } => CommandAction::Build {
            object_id,
            x,
            y,
            structure: StructureType::new(structure.as_str()),
        },
        AbiCommandAction::Repair {
            object_id,
            target_id,
        } => CommandAction::Repair {
            object_id,
            target_id,
        },
        AbiCommandAction::UpgradeController {
            object_id,
            target_id,
        } => CommandAction::UpgradeController {
            object_id,
            target_id,
        },
        AbiCommandAction::TransferToGlobal { resource, amount } => {
            CommandAction::TransferToGlobal {
                resource: resource.as_str().to_string(),
                amount,
            }
        }
        AbiCommandAction::TransferFromGlobal { resource, amount } => {
            CommandAction::TransferFromGlobal {
                resource: resource.as_str().to_string(),
                amount,
            }
        }
        AbiCommandAction::AlliedTransfer {
            target_player,
            resource,
            amount,
        } => CommandAction::AlliedTransfer {
            target_player,
            resource: resource.as_str().to_string(),
            amount,
        },
    })
}

fn action_payload_to_json(payload: abi::ActionPayload) -> serde_json::Value {
    if payload.payload.is_empty() {
        return serde_json::json!({
            "schema_hash": hex_encode(&payload.schema_hash),
        });
    }
    serde_json::from_slice(&payload.payload).unwrap_or_else(|_| {
        serde_json::json!({
            "schema_hash": hex_encode(&payload.schema_hash),
            "payload": payload.payload,
        })
    })
}

fn normalize_legacy_action(action: CommandAction) -> CommandAction {
    let Some(special) = action.special_action() else {
        return action;
    };
    CommandAction::Action {
        action_type: special.action_type.to_string(),
        object_id: special.object_id,
        target_id: Some(special.target_id),
        payload: special_action_payload(special),
    }
}

fn special_action_payload(special: SpecialActionInput<'_>) -> serde_json::Value {
    let mut payload = serde_json::Map::new();
    payload.insert(
        "target_id".to_string(),
        serde_json::json!(special.target_id),
    );
    if let Some(resource) = special.resource {
        payload.insert("resource".to_string(), serde_json::json!(resource));
    }
    if let Some(amount) = special.amount {
        payload.insert("amount".to_string(), serde_json::json!(amount));
    }
    if let Some(range) = special.range {
        payload.insert("range".to_string(), serde_json::json!(range));
    }
    if let Some(structure) = special.structure {
        payload.insert("structure".to_string(), serde_json::json!(structure));
    }
    if let Some(damage_type) = special.damage_type {
        payload.insert("damage_type".to_string(), serde_json::json!(damage_type));
    }
    if let Some(cooldown) = special.cooldown {
        payload.insert("cooldown".to_string(), serde_json::json!(cooldown));
    }
    serde_json::Value::Object(payload)
}

fn direction_from_abi(direction: abi::Direction) -> Direction {
    match direction {
        abi::Direction::North => Direction::Top,
        abi::Direction::South => Direction::Bottom,
        abi::Direction::East => Direction::TopRight,
        abi::Direction::West => Direction::BottomLeft,
    }
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
        action: normalize_legacy_action(intent.action),
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
            canonical_command_hash(command),
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

pub(crate) fn canonical_command_hash(command: &RawCommand) -> [u8; 32] {
    let mut bytes = Vec::new();
    bytes.extend_from_slice(b"swarm.command.hash.v2");
    encode_u32(&mut bytes, command.player_id);
    encode_u64(&mut bytes, command.tick);
    bytes.push(command_source_rank(command.source));
    encode_u32(&mut bytes, command.sequence);
    encode_command_auth_for_hash(&mut bytes, &command.auth);
    encode_command_action_for_hash(&mut bytes, &normalize_legacy_action(command.action.clone()));
    *blake3::hash(&bytes).as_bytes()
}

fn encode_command_auth_for_hash(bytes: &mut Vec<u8>, auth: &CommandAuth) {
    bytes.push(command_source_rank(auth.source));
    encode_u32(bytes, auth.player_id);
    encode_u64(bytes, auth.tick_submitted);
    encode_u64(bytes, auth.tick_target);
    encode_str(bytes, &auth.idempotency_key);
    encode_optional_str(bytes, auth.client_trace_id.as_deref());
    if let Some(provenance) = &auth.admin_credential_provenance {
        bytes.push(1);
        encode_str(bytes, provenance.credential_id());
        encode_str(bytes, provenance.credential_fingerprint());
        encode_str(bytes, provenance.auth_mode());
        encode_str(bytes, provenance.admin_identity());
        encode_u32(bytes, provenance.canonical_scopes().len() as u32);
        for scope in provenance.canonical_scopes() {
            encode_str(bytes, scope);
        }
    } else {
        bytes.push(0);
    }
}

fn encode_command_action_for_hash(bytes: &mut Vec<u8>, action: &CommandAction) {
    encode_str(bytes, action.wire_type());
    match action {
        CommandAction::Move {
            object_id,
            direction,
        } => {
            encode_u64(bytes, *object_id);
            encode_str(bytes, direction_name(*direction));
        }
        CommandAction::Harvest {
            object_id,
            target_id,
            resource,
        } => {
            encode_u64(bytes, *object_id);
            encode_u64(bytes, *target_id);
            encode_optional_str(bytes, resource.as_deref());
        }
        CommandAction::Transfer {
            object_id,
            target_id,
            resource,
            amount,
        }
        | CommandAction::Withdraw {
            object_id,
            target_id,
            resource,
            amount,
        } => {
            encode_u64(bytes, *object_id);
            encode_u64(bytes, *target_id);
            encode_str(bytes, resource);
            encode_u32(bytes, *amount);
        }
        CommandAction::Action {
            action_type,
            object_id,
            target_id,
            payload,
        } => {
            encode_str(bytes, action_type);
            encode_u64(bytes, *object_id);
            encode_optional_u64(bytes, *target_id);
            encode_json_value_for_hash(bytes, payload);
        }
        CommandAction::ClaimController {
            object_id,
            target_id,
        }
        | CommandAction::Repair {
            object_id,
            target_id,
        }
        | CommandAction::UpgradeController {
            object_id,
            target_id,
        } => {
            encode_u64(bytes, *object_id);
            encode_u64(bytes, *target_id);
        }
        CommandAction::Spawn {
            object_id,
            spawn_id,
            body_parts,
        } => {
            encode_u64(bytes, *object_id);
            encode_u64(bytes, *spawn_id);
            encode_u32(bytes, body_parts.len() as u32);
            for part in body_parts {
                encode_str(bytes, body_part_name(*part));
            }
        }
        CommandAction::Recycle { object_id } => encode_u64(bytes, *object_id),
        CommandAction::Build {
            object_id,
            x,
            y,
            structure,
        } => {
            encode_u64(bytes, *object_id);
            encode_i32(bytes, *x);
            encode_i32(bytes, *y);
            encode_str(bytes, structure.as_str());
        }
        CommandAction::TransferToGlobal { resource, amount }
        | CommandAction::TransferFromGlobal { resource, amount } => {
            encode_str(bytes, resource);
            encode_u32(bytes, *amount);
        }
        CommandAction::AlliedTransfer {
            target_player,
            resource,
            amount,
        } => {
            encode_u32(bytes, *target_player);
            encode_str(bytes, resource);
            encode_u32(bytes, *amount);
        }
    }
}

fn encode_json_value_for_hash(bytes: &mut Vec<u8>, value: &serde_json::Value) {
    match value {
        serde_json::Value::Null => bytes.push(0),
        serde_json::Value::Bool(value) => {
            bytes.push(1);
            bytes.push(u8::from(*value));
        }
        serde_json::Value::Number(number) => {
            bytes.push(2);
            encode_str(bytes, &number.to_string());
        }
        serde_json::Value::String(value) => {
            bytes.push(3);
            encode_str(bytes, value);
        }
        serde_json::Value::Array(items) => {
            bytes.push(4);
            encode_u32(bytes, items.len() as u32);
            for item in items {
                encode_json_value_for_hash(bytes, item);
            }
        }
        serde_json::Value::Object(fields) => {
            bytes.push(5);
            let sorted = fields.iter().collect::<BTreeMap<_, _>>();
            encode_u32(bytes, sorted.len() as u32);
            for (key, value) in sorted {
                encode_str(bytes, key);
                encode_json_value_for_hash(bytes, value);
            }
        }
    }
}

fn encode_u64(bytes: &mut Vec<u8>, value: u64) {
    bytes.extend_from_slice(&value.to_le_bytes());
}

fn encode_u32(bytes: &mut Vec<u8>, value: u32) {
    bytes.extend_from_slice(&value.to_le_bytes());
}

fn encode_i32(bytes: &mut Vec<u8>, value: i32) {
    bytes.extend_from_slice(&value.to_le_bytes());
}

fn encode_str(bytes: &mut Vec<u8>, value: &str) {
    encode_u32(bytes, value.len() as u32);
    bytes.extend_from_slice(value.as_bytes());
}

fn encode_optional_str(bytes: &mut Vec<u8>, value: Option<&str>) {
    match value {
        Some(value) => {
            bytes.push(1);
            encode_str(bytes, value);
        }
        None => bytes.push(0),
    }
}

fn encode_optional_u64(bytes: &mut Vec<u8>, value: Option<u64>) {
    match value {
        Some(value) => {
            bytes.push(1);
            encode_u64(bytes, value);
        }
        None => bytes.push(0),
    }
}

fn direction_name(direction: Direction) -> &'static str {
    match direction {
        Direction::Top => "Top",
        Direction::TopRight => "TopRight",
        Direction::BottomRight => "BottomRight",
        Direction::Bottom => "Bottom",
        Direction::BottomLeft => "BottomLeft",
        Direction::TopLeft => "TopLeft",
    }
}

fn body_part_name(part: BodyPart) -> &'static str {
    match part {
        BodyPart::Move => "Move",
        BodyPart::Work => "Work",
        BodyPart::Carry => "Carry",
        BodyPart::Attack => "Attack",
        BodyPart::RangedAttack => "RangedAttack",
        BodyPart::Heal => "Heal",
        BodyPart::Claim => "Claim",
        BodyPart::Tough => "Tough",
    }
}

fn hex_encode(bytes: &[u8]) -> String {
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
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
    raw.action
        .actor_object_id()
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

    let result = if let Some(special) = raw.action.special_action() {
        validate_special_action(world, effective_player_id, raw.tick, special)
    } else {
        match &raw.action {
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
                raw.source,
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
            } => validate_claim_controller(
                world,
                effective_player_id,
                raw.tick,
                *object_id,
                *target_id,
            ),
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
        }
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
    if let CommandAction::Action { action_type, .. } = action
        && ECONOMY_COMMAND_ACTIONS.contains(&action_type.as_str())
    {
        return true;
    }
    matches!(
        action,
        CommandAction::TransferToGlobal { .. }
            | CommandAction::TransferFromGlobal { .. }
            | CommandAction::AlliedTransfer { .. }
    )
}

fn synthetic_idempotency_key(
    source: CommandSource,
    player_id: PlayerId,
    tick: Tick,
    sequence: u32,
) -> String {
    format!("server:{source:?}:{player_id}:{tick}:{sequence}")
}

impl CommandAuth {
    pub(crate) fn server_injected(
        source: CommandSource,
        player_id: PlayerId,
        tick_submitted: Tick,
        tick_target: Tick,
    ) -> Self {
        Self::server_injected_with_intent_metadata(
            source,
            player_id,
            tick_submitted,
            tick_target,
            synthetic_idempotency_key(source, player_id, tick_target, 0),
            None,
        )
    }

    pub(crate) fn server_injected_with_intent_metadata(
        source: CommandSource,
        player_id: PlayerId,
        tick_submitted: Tick,
        tick_target: Tick,
        idempotency_key: impl Into<String>,
        client_trace_id: Option<String>,
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
            idempotency_key: idempotency_key.into(),
            client_trace_id,
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
        let source = CommandSource::Admin;
        Ok(Self {
            source,
            player_id,
            tick_submitted,
            tick_target,
            idempotency_key: synthetic_idempotency_key(source, player_id, tick_target, 0),
            client_trace_id: None,
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
        RejectionReason::InsufficientResources
        | RejectionReason::TileOccupied
        | RejectionReason::CapacityExceeded => consumed_fuel / 2,
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

pub(crate) fn canonicalize_persisted_rejection(rejection: &mut CommandRejection) {
    if !non_canonical_rejection_reason(&rejection.rejection) {
        return;
    }

    let internal_reason = format!("{:?}", rejection.rejection);
    rejection.rejection = match &rejection.rejection {
        RejectionReason::TargetNotFound
        | RejectionReason::NotMovable
        | RejectionReason::NotSource
        | RejectionReason::OrderNotFound => RejectionReason::ObjectNotFound,
        RejectionReason::FriendlyTarget
        | RejectionReason::NotFriendly
        | RejectionReason::NotYourRoom
        | RejectionReason::NotYourSpawn => RejectionReason::NotOwner,
        RejectionReason::AlreadyActed
        | RejectionReason::AlreadyFullHealth
        | RejectionReason::AlreadyHacked
        | RejectionReason::AlreadyDebilitated { .. }
        | RejectionReason::TerminalRequired => RejectionReason::CooldownActive,
        RejectionReason::NotEnoughBodyParts | RejectionReason::InsufficientMoveParts => {
            RejectionReason::NotEnoughBodyParts
        }
        RejectionReason::BodyTooLarge | RejectionReason::InvalidBodyPart => {
            RejectionReason::InvalidBodyPart
        }
        RejectionReason::TileOccupied
        | RejectionReason::InvalidTerrain
        | RejectionReason::NoPath
        | RejectionReason::PathTooLong
        | RejectionReason::OutOfRoom => RejectionReason::PositionOccupied,
        RejectionReason::InsufficientEnergy
        | RejectionReason::InsufficientResources
        | RejectionReason::CarryFull
        | RejectionReason::TargetEmpty
        | RejectionReason::ExceedsRoomCapacity
        | RejectionReason::TargetFuelTooLow => RejectionReason::InsufficientResource {
            resource: rejection
                .detail
                .get("resource")
                .and_then(serde_json::Value::as_str)
                .unwrap_or("unknown")
                .to_string(),
            required: rejection
                .detail
                .get("required")
                .and_then(serde_json::Value::as_u64)
                .and_then(|value| u32::try_from(value).ok())
                .unwrap_or_default(),
            available: rejection
                .detail
                .get("available")
                .and_then(serde_json::Value::as_u64)
                .and_then(|value| u32::try_from(value).ok())
                .unwrap_or_default(),
        },
        RejectionReason::InvalidDamageType => RejectionReason::InvalidResourceType,
        RejectionReason::TooManyConstructionSites => RejectionReason::ConstructionLimitReached,
        RejectionReason::TargetTransferLocked | RejectionReason::DailyTransferCapExceeded => {
            RejectionReason::RateLimited
        }
        RejectionReason::PlayerNotFound => RejectionReason::NotVisibleOrNotFound,
        RejectionReason::DisruptedResisted { .. } => RejectionReason::NotEnoughBodyParts,
        canonical => canonical.clone(),
    };

    let canonical_reason = canonical_rejection_reason(&rejection.rejection);
    let detail = rejection
        .detail
        .as_object_mut()
        .map(|fields| fields as &mut serde_json::Map<String, serde_json::Value>);
    if let Some(detail) = detail {
        detail.insert("reason".to_string(), serde_json::json!(canonical_reason));
        detail.insert(
            "canonical_reason".to_string(),
            serde_json::json!(canonical_reason),
        );
        detail.insert(
            "internal_reason".to_string(),
            serde_json::json!(internal_reason),
        );
    } else {
        rejection.detail = serde_json::json!({
            "reason": canonical_reason,
            "canonical_reason": canonical_reason,
            "internal_reason": internal_reason,
        });
    }
}

fn rejection_detail(command: &RawCommand, rejection: &RejectionReason) -> serde_json::Value {
    let action = command.action.wire_type();

    let detail = match rejection {
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
        RejectionReason::CapacityExceeded => match &command.action {
            CommandAction::Transfer {
                object_id,
                target_id,
                resource,
                amount,
            } => serde_json::json!({
                "reason": "CapacityExceeded",
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
                "reason": "CapacityExceeded",
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
        RejectionReason::AlreadyActed
        | RejectionReason::AlreadyFullHealth
        | RejectionReason::AlreadyHacked
        | RejectionReason::AlreadyDebilitated { .. }
        | RejectionReason::TerminalRequired => "CooldownActive",
        RejectionReason::InvalidBodyPart | RejectionReason::BodyTooLarge => "InvalidBodyPart",
        RejectionReason::NotEnoughBodyParts | RejectionReason::InsufficientMoveParts => {
            "NotEnoughBodyParts"
        }
        RejectionReason::TileOccupied
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
        | RejectionReason::TargetEmpty
        | RejectionReason::ExceedsRoomCapacity
        | RejectionReason::TargetFuelTooLow => "InsufficientResource",
        RejectionReason::CapacityExceeded => "CapacityExceeded",
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
            | RejectionReason::CapacityExceeded
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
    let action = command.raw.action;
    let result = if let Some(special) = action.special_action() {
        actor_id = Some(special.object_id);
        apply_special_action(world, player_id, action_tick, special)
    } else {
        match action {
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
                if ECONOMY_COMMAND_ACTIONS.contains(&action_type.as_str()) {
                    apply_economy_action(world, player_id, &action_type, &payload)
                } else {
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
        }
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
        return Err(RejectionReason::PositionOccupied);
    }
    if tile_has_blocking_enemy(world, target, player_id) {
        return Err(RejectionReason::PositionOccupied);
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
        return Err(RejectionReason::InsufficientResources);
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
        return Err(RejectionReason::CapacityExceeded);
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
        return Err(RejectionReason::CapacityExceeded);
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

fn validate_special_action(
    world: &mut World,
    player_id: PlayerId,
    tick: Tick,
    special: SpecialActionInput<'_>,
) -> CommandResult {
    match special.action_type {
        "Attack" => validate_attack(world, player_id, tick, special.object_id, special.target_id),
        "RangedAttack" => validate_ranged_attack(
            world,
            player_id,
            tick,
            special.object_id,
            special.target_id,
            special.range.unwrap_or(MAX_RANGED_ATTACK_RANGE),
        ),
        "Heal" => validate_heal(world, player_id, tick, special.object_id, special.target_id),
        action_type => validate_custom_action(
            world,
            player_id,
            tick,
            action_type,
            special.object_id,
            Some(special.target_id),
        ),
    }
}

fn validate_action(
    source: CommandSource,
    world: &mut World,
    player_id: PlayerId,
    tick: Tick,
    action_type: &str,
    object_id: ObjectId,
    target_id: Option<ObjectId>,
    payload: &serde_json::Value,
) -> CommandResult {
    if is_core_action(action_type) || action_type == "Action" {
        return Err(RejectionReason::UnknownAction {
            action: action_type.to_string(),
        });
    }
    if ECONOMY_COMMAND_ACTIONS.contains(&action_type) {
        return validate_economy_action(source, world, player_id, action_type, payload);
    }
    let target_id = target_id.or_else(|| payload_target_id(payload));
    match action_type {
        "Attack" => validate_required_target(target_id)
            .and_then(|target_id| validate_attack(world, player_id, tick, object_id, target_id)),
        "RangedAttack" => validate_required_target(target_id).and_then(|target_id| {
            validate_ranged_attack(
                world,
                player_id,
                tick,
                object_id,
                target_id,
                payload_range(payload).unwrap_or(MAX_RANGED_ATTACK_RANGE),
            )
        }),
        "Heal" => validate_required_target(target_id)
            .and_then(|target_id| validate_heal(world, player_id, tick, object_id, target_id)),
        _ => validate_custom_action(world, player_id, tick, action_type, object_id, target_id),
    }
}

fn payload_target_id(payload: &serde_json::Value) -> Option<ObjectId> {
    payload.get("target_id").and_then(serde_json::Value::as_u64)
}

fn payload_range(payload: &serde_json::Value) -> Option<u32> {
    payload
        .get("range")
        .and_then(serde_json::Value::as_u64)
        .and_then(|value| u32::try_from(value).ok())
}

fn validate_required_target(target_id: Option<ObjectId>) -> Result<ObjectId, RejectionReason> {
    target_id.ok_or_else(|| RejectionReason::UnknownAction {
        action: "missing target_id".to_string(),
    })
}

fn required_payload_field<T>(payload: &serde_json::Value, field: &str) -> Result<T, RejectionReason>
where
    T: DeserializeOwned,
{
    let value = payload
        .get(field)
        .ok_or(RejectionReason::SchemaViolation)?
        .clone();
    serde_json::from_value(value).map_err(|_| RejectionReason::SchemaViolation)
}

fn optional_payload_field<T>(
    payload: &serde_json::Value,
    field: &str,
) -> Result<Option<T>, RejectionReason>
where
    T: DeserializeOwned,
{
    payload
        .get(field)
        .cloned()
        .map(serde_json::from_value)
        .transpose()
        .map_err(|_| RejectionReason::SchemaViolation)
}

fn validate_economy_action(
    source: CommandSource,
    world: &World,
    player_id: PlayerId,
    action_type: &str,
    payload: &serde_json::Value,
) -> CommandResult {
    match action_type {
        "CreateContractSettlement" => validate_create_contract(
            world,
            player_id,
            required_payload_field(payload, "settlement_id")?,
            required_payload_field(payload, "nonce")?,
            &required_payload_field::<String>(payload, "input_resource")?,
            required_payload_field(payload, "input_amount")?,
            &required_payload_field::<String>(payload, "output_resource")?,
            required_payload_field(payload, "output_amount")?,
            optional_payload_field(payload, "counterparty")?,
            optional_payload_field(payload, "expires_at")?,
        ),
        "SettleContract" => validate_settle_contract(
            world,
            player_id,
            required_payload_field(payload, "settlement_id")?,
        ),
        "CancelContract" => validate_cancel_contract(
            world,
            player_id,
            required_payload_field(payload, "settlement_id")?,
        ),
        "CreateMerchantQuote" => validate_create_merchant_quote(
            source,
            world,
            player_id,
            required_payload_field(payload, "quote_id")?,
            required_payload_field(payload, "player_id")?,
            &required_payload_field::<String>(payload, "pay_resource")?,
            required_payload_field(payload, "pay_amount")?,
            &required_payload_field::<String>(payload, "receive_resource")?,
            required_payload_field(payload, "receive_amount")?,
            required_payload_field(payload, "expires_at")?,
        ),
        "AcceptMerchantTrade" => validate_accept_merchant_trade(
            world,
            player_id,
            required_payload_field(payload, "quote_id")?,
            required_payload_field(payload, "min_receive")?,
        ),
        "CreateP2POffer" => validate_create_p2p_offer(
            world,
            player_id,
            required_payload_field(payload, "offer_id")?,
            required_payload_field(payload, "nonce")?,
            &required_payload_field::<String>(payload, "give_resource")?,
            required_payload_field(payload, "give_amount")?,
            &required_payload_field::<String>(payload, "want_resource")?,
            required_payload_field(payload, "want_amount")?,
            required_payload_field(payload, "expires_at")?,
        ),
        "AcceptP2POffer" => validate_accept_p2p_offer(
            world,
            player_id,
            required_payload_field(payload, "offer_id")?,
        ),
        "CancelP2POffer" => validate_cancel_p2p_offer(
            world,
            player_id,
            required_payload_field(payload, "offer_id")?,
        ),
        "RefundP2POffer" => validate_refund_p2p_offer(
            world,
            player_id,
            required_payload_field(payload, "offer_id")?,
        ),
        "CreateAuction" => validate_create_auction(
            world,
            player_id,
            required_payload_field(payload, "auction_id")?,
            required_payload_field(payload, "nonce")?,
            &required_payload_field::<String>(payload, "lot_resource")?,
            required_payload_field(payload, "lot_amount")?,
            &required_payload_field::<String>(payload, "bid_resource")?,
            required_payload_field(payload, "min_bid")?,
            required_payload_field(payload, "ends_at")?,
        ),
        "BidAuction" => validate_bid_auction(
            world,
            player_id,
            required_payload_field(payload, "auction_id")?,
            required_payload_field(payload, "bid_amount")?,
        ),
        "SettleAuction" => {
            validate_settle_auction(world, required_payload_field(payload, "auction_id")?)
        }
        "CancelAuction" => validate_cancel_auction(
            world,
            player_id,
            required_payload_field(payload, "auction_id")?,
        ),
        "CreateEscrow" => validate_create_escrow(
            world,
            player_id,
            required_payload_field(payload, "escrow_id")?,
            required_payload_field(payload, "nonce")?,
            required_payload_field(payload, "payee")?,
            required_payload_field(payload, "arbiter")?,
            &required_payload_field::<String>(payload, "resource")?,
            required_payload_field(payload, "amount")?,
        ),
        "ReleaseEscrow" => validate_release_escrow(
            world,
            player_id,
            required_payload_field(payload, "escrow_id")?,
        ),
        "RefundEscrow" => validate_refund_escrow(
            world,
            player_id,
            required_payload_field(payload, "escrow_id")?,
        ),
        "CreateLoanOffer" => validate_create_loan_offer(
            world,
            player_id,
            required_payload_field(payload, "loan_id")?,
            required_payload_field(payload, "nonce")?,
            required_payload_field(payload, "borrower")?,
            &required_payload_field::<String>(payload, "resource")?,
            required_payload_field(payload, "principal")?,
            required_payload_field(payload, "repay_amount")?,
            required_payload_field(payload, "due_at")?,
        ),
        "AcceptLoan" => validate_accept_loan(
            world,
            player_id,
            required_payload_field(payload, "loan_id")?,
        ),
        "RepayLoan" => validate_repay_loan(
            world,
            player_id,
            required_payload_field(payload, "loan_id")?,
        ),
        "DefaultLoan" => validate_default_loan(world, required_payload_field(payload, "loan_id")?),
        unknown => Err(RejectionReason::UnknownAction {
            action: unknown.to_string(),
        }),
    }
}

fn apply_economy_action(
    world: &mut World,
    player_id: PlayerId,
    action_type: &str,
    payload: &serde_json::Value,
) -> CommandResult {
    match action_type {
        "CreateContractSettlement" => apply_create_contract(
            world,
            player_id,
            required_payload_field(payload, "settlement_id")?,
            required_payload_field(payload, "nonce")?,
            &required_payload_field::<String>(payload, "input_resource")?,
            required_payload_field(payload, "input_amount")?,
            &required_payload_field::<String>(payload, "output_resource")?,
            required_payload_field(payload, "output_amount")?,
            optional_payload_field(payload, "counterparty")?,
            optional_payload_field(payload, "expires_at")?,
        ),
        "SettleContract" => apply_settle_contract(
            world,
            player_id,
            required_payload_field(payload, "settlement_id")?,
        ),
        "CancelContract" => apply_cancel_contract(
            world,
            player_id,
            required_payload_field(payload, "settlement_id")?,
        ),
        "CreateMerchantQuote" => apply_create_merchant_quote(
            world,
            required_payload_field(payload, "quote_id")?,
            required_payload_field(payload, "player_id")?,
            &required_payload_field::<String>(payload, "pay_resource")?,
            required_payload_field(payload, "pay_amount")?,
            &required_payload_field::<String>(payload, "receive_resource")?,
            required_payload_field(payload, "receive_amount")?,
            required_payload_field(payload, "expires_at")?,
        ),
        "AcceptMerchantTrade" => apply_accept_merchant_trade(
            world,
            player_id,
            required_payload_field(payload, "quote_id")?,
            required_payload_field(payload, "min_receive")?,
        ),
        "CreateP2POffer" => apply_create_p2p_offer(
            world,
            player_id,
            required_payload_field(payload, "offer_id")?,
            required_payload_field(payload, "nonce")?,
            &required_payload_field::<String>(payload, "give_resource")?,
            required_payload_field(payload, "give_amount")?,
            &required_payload_field::<String>(payload, "want_resource")?,
            required_payload_field(payload, "want_amount")?,
            required_payload_field(payload, "expires_at")?,
        ),
        "AcceptP2POffer" => apply_accept_p2p_offer(
            world,
            player_id,
            required_payload_field(payload, "offer_id")?,
        ),
        "CancelP2POffer" => apply_cancel_p2p_offer(
            world,
            player_id,
            required_payload_field(payload, "offer_id")?,
        ),
        "RefundP2POffer" => apply_refund_p2p_offer(
            world,
            player_id,
            required_payload_field(payload, "offer_id")?,
        ),
        "CreateAuction" => apply_create_auction(
            world,
            player_id,
            required_payload_field(payload, "auction_id")?,
            required_payload_field(payload, "nonce")?,
            &required_payload_field::<String>(payload, "lot_resource")?,
            required_payload_field(payload, "lot_amount")?,
            &required_payload_field::<String>(payload, "bid_resource")?,
            required_payload_field(payload, "min_bid")?,
            required_payload_field(payload, "ends_at")?,
        ),
        "BidAuction" => apply_bid_auction(
            world,
            player_id,
            required_payload_field(payload, "auction_id")?,
            required_payload_field(payload, "bid_amount")?,
        ),
        "SettleAuction" => {
            apply_settle_auction(world, required_payload_field(payload, "auction_id")?)
        }
        "CancelAuction" => apply_cancel_auction(
            world,
            player_id,
            required_payload_field(payload, "auction_id")?,
        ),
        "CreateEscrow" => apply_create_escrow(
            world,
            player_id,
            required_payload_field(payload, "escrow_id")?,
            required_payload_field(payload, "nonce")?,
            required_payload_field(payload, "payee")?,
            required_payload_field(payload, "arbiter")?,
            &required_payload_field::<String>(payload, "resource")?,
            required_payload_field(payload, "amount")?,
        ),
        "ReleaseEscrow" => apply_release_escrow(
            world,
            player_id,
            required_payload_field(payload, "escrow_id")?,
        ),
        "RefundEscrow" => apply_refund_escrow(
            world,
            player_id,
            required_payload_field(payload, "escrow_id")?,
        ),
        "CreateLoanOffer" => apply_create_loan_offer(
            world,
            player_id,
            required_payload_field(payload, "loan_id")?,
            required_payload_field(payload, "nonce")?,
            required_payload_field(payload, "borrower")?,
            &required_payload_field::<String>(payload, "resource")?,
            required_payload_field(payload, "principal")?,
            required_payload_field(payload, "repay_amount")?,
            required_payload_field(payload, "due_at")?,
        ),
        "AcceptLoan" => apply_accept_loan(
            world,
            player_id,
            required_payload_field(payload, "loan_id")?,
        ),
        "RepayLoan" => apply_repay_loan(
            world,
            player_id,
            required_payload_field(payload, "loan_id")?,
        ),
        "DefaultLoan" => apply_default_loan(world, required_payload_field(payload, "loan_id")?),
        unknown => Err(RejectionReason::UnknownAction {
            action: unknown.to_string(),
        }),
    }
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
    if !world
        .resource::<RoomTerrains>()
        .is_passable(spawn_output_position(position))
    {
        return Err(RejectionReason::InvalidTerrain);
    }
    ensure_player_resource_cost(world, player_id, &cost, true)?;
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
    require_body(&drone, BodyPart::Carry)?;
    let carried_energy = drone
        .carry
        .get(ENERGY_RESOURCE)
        .copied()
        .unwrap_or_default();
    if carried_energy == 0 {
        return Err(RejectionReason::InsufficientResource {
            resource: ENERGY_RESOURCE.to_string(),
            required: 1,
            available: 0,
        });
    }
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
        return Err(RejectionReason::CapacityExceeded);
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
        return Err(RejectionReason::CooldownActive);
    }
    if drone.fatigue > 0 {
        return Err(RejectionReason::CooldownActive);
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
                return Err(RejectionReason::CapacityExceeded);
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

fn apply_special_action(
    world: &mut World,
    player_id: PlayerId,
    tick: Tick,
    special: SpecialActionInput<'_>,
) -> CommandResult {
    match special.action_type {
        "Attack" => apply_basic_attack(world, special.object_id, special.target_id),
        "RangedAttack" => apply_basic_ranged_attack(world, special.object_id, special.target_id),
        "Heal" => apply_basic_heal(world, special.object_id, special.target_id),
        action_type => apply_custom_action(
            world,
            player_id,
            tick,
            action_type,
            special.object_id,
            Some(special.target_id),
            special.structure,
        ),
    }
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
    if is_core_or_economy_action(action_type) || action_type == "Action" {
        return Err(RejectionReason::UnknownAction {
            action: action_type.to_string(),
        });
    }
    let target_id = target_id.or_else(|| payload_target_id(payload));
    match action_type {
        "Attack" => {
            return validate_required_target(target_id)
                .and_then(|target_id| apply_basic_attack(world, object_id, target_id));
        }
        "RangedAttack" => {
            return validate_required_target(target_id)
                .and_then(|target_id| apply_basic_ranged_attack(world, object_id, target_id));
        }
        "Heal" => {
            return validate_required_target(target_id)
                .and_then(|target_id| apply_basic_heal(world, object_id, target_id));
        }
        _ => {}
    }
    apply_custom_action(
        world,
        player_id,
        tick,
        action_type,
        object_id,
        target_id,
        payload_structure(payload),
    )
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
            crate::systems::combat_system::final_damage_multiplier_bps(
                Some(&drone.body),
                attrs,
                flags,
                damage_type,
                body_registry,
                damage_registry,
                resistance_registry,
            )
        } else if entity_ref.get::<Structure>().is_some() {
            crate::systems::combat_system::final_damage_multiplier_bps(
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
    let damage = scale_damage_bps(damage, multiplier);
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
    }
    deduct_player_resource_cost(world, player_id, &cost, true);
    record_resource_cost(world, player_id, &cost, false, ResourceOperation::SpawnCost);
    world
        .resource_mut::<PendingSpawnQueue>()
        .0
        .push(PendingSpawn {
            owner: player_id,
            spawn_id,
            body,
            position: spawn_output_position(position),
            cost,
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
    world
        .resource_mut::<PendingHeal>()
        .push(target, repair_amount);
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
            let heal = scale_micro(dealt, action.special_param_micro.unwrap_or(500_000));
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
) -> Result<u32, RejectionReason> {
    let body_registry = world.resource::<BodyPartRegistry>();
    let damage_registry = world.resource::<DamageTypeRegistry>();
    let resistance_registry = world.resource::<ResistanceRegistry>();
    let entity_ref = world
        .get_entity(target)
        .map_err(|_| RejectionReason::ObjectNotFound)?;
    let attrs = entity_ref.get::<Attributes>();
    let flags = entity_ref.get::<EntityFlags>();
    if let Some(drone) = entity_ref.get::<Drone>() {
        Ok(crate::systems::combat_system::final_damage_multiplier_bps(
            Some(&drone.body),
            attrs,
            flags,
            damage_type,
            body_registry,
            damage_registry,
            resistance_registry,
        ))
    } else if entity_ref.get::<Structure>().is_some() {
        Ok(crate::systems::combat_system::final_damage_multiplier_bps(
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

fn scale_damage_bps(amount: u32, multiplier_bps: u32) -> u32 {
    ((amount as u64 * multiplier_bps as u64) / 10_000).min(u32::MAX as u64) as u32
}

fn scale_micro(amount: u32, multiplier_micro: u64) -> u32 {
    ((amount as u64).saturating_mul(multiplier_micro) / 1_000_000).min(u32::MAX as u64) as u32
}

fn apply_resisted_damage_amount(
    world: &mut World,
    target_id: ObjectId,
    damage_type: &str,
    damage: u32,
) -> Result<u32, RejectionReason> {
    let target = entity(target_id)?;
    let multiplier = effect_multiplier(world, target, damage_type)?;
    let damage = scale_damage_bps(damage, multiplier);
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
        amount,
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

#[allow(clippy::too_many_arguments)]
fn validate_create_contract(
    world: &World,
    owner: PlayerId,
    id: SettlementId,
    nonce: u64,
    input_resource: &str,
    input_amount: u32,
    output_resource: &str,
    output_amount: u32,
    _counterparty: Option<PlayerId>,
    expires_at: Option<Tick>,
) -> CommandResult {
    ensure_positive(input_amount)?;
    ensure_positive(output_amount)?;
    ensure_valid_resource_names(input_resource, output_resource)?;
    if input_resource == output_resource && output_amount > input_amount {
        return Err(RejectionReason::InsufficientResources);
    }
    ensure_not_expired(world, expires_at)?;
    ensure_settlement_id_available(world, id, owner, nonce)
}

#[allow(clippy::too_many_arguments)]
fn apply_create_contract(
    world: &mut World,
    owner: PlayerId,
    id: SettlementId,
    nonce: u64,
    input_resource: &str,
    input_amount: u32,
    output_resource: &str,
    output_amount: u32,
    counterparty: Option<PlayerId>,
    expires_at: Option<Tick>,
) -> CommandResult {
    validate_create_contract(
        world,
        owner,
        id,
        nonce,
        input_resource,
        input_amount,
        output_resource,
        output_amount,
        counterparty,
        expires_at,
    )?;
    reserve_player_resource(
        world,
        SettlementKind::Contract,
        id,
        owner,
        input_resource,
        input_amount,
    )?;
    world
        .resource_mut::<SettlementState>()
        .id_nonces
        .insert(id, SettlementIdNonce { owner, nonce });
    world.resource_mut::<SettlementState>().contracts.insert(
        id,
        ContractSettlement {
            owner,
            counterparty,
            input_resource: input_resource.to_string(),
            input_amount,
            output_resource: output_resource.to_string(),
            output_amount,
            expires_at,
            status: SettlementStatus::Open,
        },
    );
    mark_receipt(
        world,
        SettlementKind::Contract,
        id,
        SettlementPhase::Reserve,
    );
    record_account_flow(
        world,
        player_account(owner),
        reserve_account(SettlementKind::Contract, id, "input"),
        input_resource,
        input_amount,
        ResourceOperation::plugin_settlement(PluginSettlementKind::Contract),
    );
    Ok(())
}

fn validate_settle_contract(world: &World, player: PlayerId, id: SettlementId) -> CommandResult {
    let contract = world
        .resource::<SettlementState>()
        .contracts
        .get(&id)
        .ok_or(RejectionReason::OrderNotFound)?;
    if contract.owner != player && contract.counterparty != Some(player) {
        return Err(RejectionReason::NotOwner);
    }
    if has_receipt(world, SettlementKind::Contract, id, SettlementPhase::Settle) {
        return Ok(());
    }
    ensure_open(contract.status)?;
    ensure_not_expired(world, contract.expires_at)
}

fn apply_settle_contract(world: &mut World, player: PlayerId, id: SettlementId) -> CommandResult {
    validate_settle_contract(world, player, id)?;
    if has_receipt(world, SettlementKind::Contract, id, SettlementPhase::Settle) {
        return Ok(());
    }
    let contract = world
        .resource::<SettlementState>()
        .contracts
        .get(&id)
        .cloned()
        .ok_or(RejectionReason::OrderNotFound)?;
    if contract.input_resource == contract.output_resource {
        release_reserved_to_player(
            world,
            SettlementKind::Contract,
            id,
            contract.owner,
            &contract.output_resource,
            contract.output_amount,
        )?;
        record_account_flow(
            world,
            reserve_account(SettlementKind::Contract, id, "input"),
            player_account(contract.owner),
            &contract.output_resource,
            contract.output_amount,
            ResourceOperation::plugin_settlement(PluginSettlementKind::Contract),
        );
        if contract.input_amount > contract.output_amount {
            consume_reserved(
                world,
                SettlementKind::Contract,
                id,
                &contract.input_resource,
                contract.input_amount - contract.output_amount,
            )?;
            record_account_flow(
                world,
                reserve_account(SettlementKind::Contract, id, "input"),
                settlement_sink("contract_remainder"),
                &contract.input_resource,
                contract.input_amount - contract.output_amount,
                ResourceOperation::plugin_settlement(PluginSettlementKind::Contract),
            );
        }
    } else {
        consume_reserved(
            world,
            SettlementKind::Contract,
            id,
            &contract.input_resource,
            contract.input_amount,
        )?;
        credit_global(
            world,
            contract.owner,
            &contract.output_resource,
            contract.output_amount,
        );
        record_account_flow(
            world,
            reserve_account(SettlementKind::Contract, id, "input"),
            settlement_sink("contract_input"),
            &contract.input_resource,
            contract.input_amount,
            ResourceOperation::plugin_settlement(PluginSettlementKind::Contract),
        );
        record_account_flow(
            world,
            settlement_system("contract_output"),
            player_account(contract.owner),
            &contract.output_resource,
            contract.output_amount,
            ResourceOperation::plugin_settlement(PluginSettlementKind::Contract),
        );
    }
    world
        .resource_mut::<SettlementState>()
        .contracts
        .get_mut(&id)
        .unwrap()
        .status = SettlementStatus::Settled;
    mark_receipt(world, SettlementKind::Contract, id, SettlementPhase::Settle);
    Ok(())
}

fn validate_cancel_contract(world: &World, player: PlayerId, id: SettlementId) -> CommandResult {
    let contract = world
        .resource::<SettlementState>()
        .contracts
        .get(&id)
        .ok_or(RejectionReason::OrderNotFound)?;
    if contract.owner != player {
        return Err(RejectionReason::NotOwner);
    }
    if has_receipt(world, SettlementKind::Contract, id, SettlementPhase::Cancel) {
        return Ok(());
    }
    ensure_open(contract.status)?;
    Ok(())
}

fn apply_cancel_contract(world: &mut World, player: PlayerId, id: SettlementId) -> CommandResult {
    validate_cancel_contract(world, player, id)?;
    if has_receipt(world, SettlementKind::Contract, id, SettlementPhase::Cancel) {
        return Ok(());
    }
    let contract = world
        .resource::<SettlementState>()
        .contracts
        .get(&id)
        .cloned()
        .ok_or(RejectionReason::OrderNotFound)?;
    release_reserved_to_player(
        world,
        SettlementKind::Contract,
        id,
        contract.owner,
        &contract.input_resource,
        contract.input_amount,
    )?;
    world
        .resource_mut::<SettlementState>()
        .contracts
        .get_mut(&id)
        .unwrap()
        .status = SettlementStatus::Cancelled;
    mark_receipt(world, SettlementKind::Contract, id, SettlementPhase::Cancel);
    record_account_flow(
        world,
        reserve_account(SettlementKind::Contract, id, "input"),
        player_account(contract.owner),
        &contract.input_resource,
        contract.input_amount,
        ResourceOperation::plugin_settlement(PluginSettlementKind::Contract),
    );
    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn validate_create_merchant_quote(
    source: CommandSource,
    world: &World,
    actor: PlayerId,
    quote_id: SettlementId,
    player_id: PlayerId,
    pay_resource: &str,
    pay_amount: u32,
    receive_resource: &str,
    receive_amount: u32,
    expires_at: Tick,
) -> CommandResult {
    if !matches!(source, CommandSource::Admin | CommandSource::TestHarness) || actor != 0 {
        return Err(RejectionReason::SourceNotAllowed);
    }
    ensure_positive(pay_amount)?;
    ensure_positive(receive_amount)?;
    ensure_valid_resource_names(pay_resource, receive_resource)?;
    if world.resource::<CurrentTick>().0 >= expires_at {
        return Err(RejectionReason::OrderNotFound);
    }
    if world
        .resource::<SettlementState>()
        .merchant_quotes
        .contains_key(&quote_id)
    {
        return Err(RejectionReason::OrderNotFound);
    }
    let _ = player_id;
    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn apply_create_merchant_quote(
    world: &mut World,
    quote_id: SettlementId,
    player_id: PlayerId,
    pay_resource: &str,
    pay_amount: u32,
    receive_resource: &str,
    receive_amount: u32,
    expires_at: Tick,
) -> CommandResult {
    world
        .resource_mut::<SettlementState>()
        .merchant_quotes
        .insert(
            quote_id,
            MerchantQuote {
                player_id,
                pay_resource: pay_resource.to_string(),
                pay_amount,
                receive_resource: receive_resource.to_string(),
                receive_amount,
                expires_at,
                status: SettlementStatus::Open,
            },
        );
    Ok(())
}

fn validate_accept_merchant_trade(
    world: &World,
    player: PlayerId,
    quote_id: SettlementId,
    min_receive: u32,
) -> CommandResult {
    let quote = world
        .resource::<SettlementState>()
        .merchant_quotes
        .get(&quote_id)
        .ok_or(RejectionReason::OrderNotFound)?;
    if quote.player_id != player {
        return Err(RejectionReason::OrderNotFound);
    }
    if has_receipt(
        world,
        SettlementKind::MerchantTrade,
        quote_id,
        SettlementPhase::Settle,
    ) {
        return Ok(());
    }
    ensure_open(quote.status)?;
    if quote.receive_amount < min_receive || world.resource::<CurrentTick>().0 > quote.expires_at {
        return Err(RejectionReason::OrderNotFound);
    }
    ensure_player_global_resource(world, player, &quote.pay_resource, quote.pay_amount)
}

fn apply_accept_merchant_trade(
    world: &mut World,
    player: PlayerId,
    quote_id: SettlementId,
    min_receive: u32,
) -> CommandResult {
    validate_accept_merchant_trade(world, player, quote_id, min_receive)?;
    if has_receipt(
        world,
        SettlementKind::MerchantTrade,
        quote_id,
        SettlementPhase::Settle,
    ) {
        return Ok(());
    }
    let quote = world
        .resource::<SettlementState>()
        .merchant_quotes
        .get(&quote_id)
        .cloned()
        .ok_or(RejectionReason::OrderNotFound)?;
    debit_global(world, player, &quote.pay_resource, quote.pay_amount)?;
    credit_global(world, player, &quote.receive_resource, quote.receive_amount);
    world
        .resource_mut::<SettlementState>()
        .merchant_quotes
        .get_mut(&quote_id)
        .unwrap()
        .status = SettlementStatus::Settled;
    mark_receipt(
        world,
        SettlementKind::MerchantTrade,
        quote_id,
        SettlementPhase::Settle,
    );
    record_account_flow(
        world,
        player_account(player),
        merchant_account(quote_id),
        &quote.pay_resource,
        quote.pay_amount,
        ResourceOperation::plugin_settlement(PluginSettlementKind::MerchantTrade),
    );
    record_account_flow(
        world,
        merchant_account(quote_id),
        player_account(player),
        &quote.receive_resource,
        quote.receive_amount,
        ResourceOperation::plugin_settlement(PluginSettlementKind::MerchantTrade),
    );
    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn validate_create_p2p_offer(
    world: &World,
    maker: PlayerId,
    id: SettlementId,
    nonce: u64,
    give_resource: &str,
    give_amount: u32,
    want_resource: &str,
    want_amount: u32,
    expires_at: Tick,
) -> CommandResult {
    ensure_positive(give_amount)?;
    ensure_positive(want_amount)?;
    ensure_valid_resource_names(give_resource, want_resource)?;
    if world.resource::<CurrentTick>().0 >= expires_at {
        return Err(RejectionReason::OrderNotFound);
    }
    ensure_settlement_id_available(world, id, maker, nonce)?;
    ensure_player_global_resource(world, maker, give_resource, give_amount)
}

#[allow(clippy::too_many_arguments)]
fn apply_create_p2p_offer(
    world: &mut World,
    maker: PlayerId,
    id: SettlementId,
    nonce: u64,
    give_resource: &str,
    give_amount: u32,
    want_resource: &str,
    want_amount: u32,
    expires_at: Tick,
) -> CommandResult {
    validate_create_p2p_offer(
        world,
        maker,
        id,
        nonce,
        give_resource,
        give_amount,
        want_resource,
        want_amount,
        expires_at,
    )?;
    reserve_player_resource(
        world,
        SettlementKind::P2POffer,
        id,
        maker,
        give_resource,
        give_amount,
    )?;
    world.resource_mut::<SettlementState>().id_nonces.insert(
        id,
        SettlementIdNonce {
            owner: maker,
            nonce,
        },
    );
    world.resource_mut::<SettlementState>().p2p_offers.insert(
        id,
        P2POffer {
            maker,
            taker: None,
            give_resource: give_resource.to_string(),
            give_amount,
            want_resource: want_resource.to_string(),
            want_amount,
            expires_at,
            status: SettlementStatus::Open,
        },
    );
    mark_receipt(
        world,
        SettlementKind::P2POffer,
        id,
        SettlementPhase::Reserve,
    );
    record_account_flow(
        world,
        player_account(maker),
        reserve_account(SettlementKind::P2POffer, id, "give"),
        give_resource,
        give_amount,
        ResourceOperation::plugin_settlement(PluginSettlementKind::P2POffer),
    );
    Ok(())
}

fn validate_accept_p2p_offer(world: &World, taker: PlayerId, id: SettlementId) -> CommandResult {
    let offer = world
        .resource::<SettlementState>()
        .p2p_offers
        .get(&id)
        .ok_or(RejectionReason::OrderNotFound)?;
    if has_receipt(world, SettlementKind::P2POffer, id, SettlementPhase::Accept) {
        return if offer.taker == Some(taker) {
            Ok(())
        } else {
            Err(RejectionReason::NotOwner)
        };
    }
    ensure_open(offer.status)?;
    if offer.maker == taker || world.resource::<CurrentTick>().0 > offer.expires_at {
        return Err(RejectionReason::OrderNotFound);
    }
    ensure_player_global_resource(world, taker, &offer.want_resource, offer.want_amount)
}

fn apply_accept_p2p_offer(world: &mut World, taker: PlayerId, id: SettlementId) -> CommandResult {
    validate_accept_p2p_offer(world, taker, id)?;
    if has_receipt(world, SettlementKind::P2POffer, id, SettlementPhase::Accept) {
        return Ok(());
    }
    let offer = world
        .resource::<SettlementState>()
        .p2p_offers
        .get(&id)
        .cloned()
        .ok_or(RejectionReason::OrderNotFound)?;
    debit_global(world, taker, &offer.want_resource, offer.want_amount)?;
    credit_global(world, offer.maker, &offer.want_resource, offer.want_amount);
    release_reserved_to_player(
        world,
        SettlementKind::P2POffer,
        id,
        taker,
        &offer.give_resource,
        offer.give_amount,
    )?;
    let mut stored = world.resource_mut::<SettlementState>();
    let offer_mut = stored.p2p_offers.get_mut(&id).unwrap();
    offer_mut.taker = Some(taker);
    offer_mut.status = SettlementStatus::Settled;
    mark_receipt(world, SettlementKind::P2POffer, id, SettlementPhase::Accept);
    record_account_flow(
        world,
        player_account(taker),
        player_account(offer.maker),
        &offer.want_resource,
        offer.want_amount,
        ResourceOperation::plugin_settlement(PluginSettlementKind::P2POffer),
    );
    record_account_flow(
        world,
        reserve_account(SettlementKind::P2POffer, id, "give"),
        player_account(taker),
        &offer.give_resource,
        offer.give_amount,
        ResourceOperation::plugin_settlement(PluginSettlementKind::P2POffer),
    );
    Ok(())
}

fn validate_cancel_p2p_offer(world: &World, maker: PlayerId, id: SettlementId) -> CommandResult {
    let offer = world
        .resource::<SettlementState>()
        .p2p_offers
        .get(&id)
        .ok_or(RejectionReason::OrderNotFound)?;
    if offer.maker != maker {
        return Err(RejectionReason::NotOwner);
    }
    ensure_open(offer.status)
}

fn apply_cancel_p2p_offer(world: &mut World, maker: PlayerId, id: SettlementId) -> CommandResult {
    validate_cancel_p2p_offer(world, maker, id)?;
    refund_p2p_offer(world, id, SettlementPhase::Cancel)
}
fn validate_refund_p2p_offer(world: &World, maker: PlayerId, id: SettlementId) -> CommandResult {
    let offer = world
        .resource::<SettlementState>()
        .p2p_offers
        .get(&id)
        .ok_or(RejectionReason::OrderNotFound)?;
    if offer.maker != maker {
        return Err(RejectionReason::NotOwner);
    }
    if has_receipt(world, SettlementKind::P2POffer, id, SettlementPhase::Refund) {
        return Ok(());
    }
    ensure_open(offer.status)?;
    if world.resource::<CurrentTick>().0 <= offer.expires_at {
        return Err(RejectionReason::OrderNotFound);
    }
    Ok(())
}
fn apply_refund_p2p_offer(world: &mut World, maker: PlayerId, id: SettlementId) -> CommandResult {
    validate_refund_p2p_offer(world, maker, id)?;
    refund_p2p_offer(world, id, SettlementPhase::Refund)
}
fn refund_p2p_offer(world: &mut World, id: SettlementId, phase: SettlementPhase) -> CommandResult {
    if has_receipt(world, SettlementKind::P2POffer, id, phase) {
        return Ok(());
    }
    let offer = world
        .resource::<SettlementState>()
        .p2p_offers
        .get(&id)
        .cloned()
        .ok_or(RejectionReason::OrderNotFound)?;
    release_reserved_to_player(
        world,
        SettlementKind::P2POffer,
        id,
        offer.maker,
        &offer.give_resource,
        offer.give_amount,
    )?;
    world
        .resource_mut::<SettlementState>()
        .p2p_offers
        .get_mut(&id)
        .unwrap()
        .status = if phase == SettlementPhase::Refund {
        SettlementStatus::Refunded
    } else {
        SettlementStatus::Cancelled
    };
    mark_receipt(world, SettlementKind::P2POffer, id, phase);
    record_account_flow(
        world,
        reserve_account(SettlementKind::P2POffer, id, "give"),
        player_account(offer.maker),
        &offer.give_resource,
        offer.give_amount,
        ResourceOperation::plugin_settlement(PluginSettlementKind::P2POffer),
    );
    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn validate_create_auction(
    world: &World,
    seller: PlayerId,
    id: SettlementId,
    nonce: u64,
    lot_resource: &str,
    lot_amount: u32,
    bid_resource: &str,
    min_bid: u32,
    ends_at: Tick,
) -> CommandResult {
    ensure_positive(lot_amount)?;
    ensure_positive(min_bid)?;
    ensure_valid_resource_names(lot_resource, bid_resource)?;
    if world.resource::<CurrentTick>().0 >= ends_at {
        return Err(RejectionReason::OrderNotFound);
    }
    ensure_settlement_id_available(world, id, seller, nonce)?;
    ensure_player_global_resource(world, seller, lot_resource, lot_amount)
}

#[allow(clippy::too_many_arguments)]
fn apply_create_auction(
    world: &mut World,
    seller: PlayerId,
    id: SettlementId,
    nonce: u64,
    lot_resource: &str,
    lot_amount: u32,
    bid_resource: &str,
    min_bid: u32,
    ends_at: Tick,
) -> CommandResult {
    validate_create_auction(
        world,
        seller,
        id,
        nonce,
        lot_resource,
        lot_amount,
        bid_resource,
        min_bid,
        ends_at,
    )?;
    reserve_player_resource(
        world,
        SettlementKind::Auction,
        id,
        seller,
        lot_resource,
        lot_amount,
    )?;
    world.resource_mut::<SettlementState>().id_nonces.insert(
        id,
        SettlementIdNonce {
            owner: seller,
            nonce,
        },
    );
    world.resource_mut::<SettlementState>().auctions.insert(
        id,
        AuctionSettlement {
            seller,
            lot_resource: lot_resource.to_string(),
            lot_amount,
            bid_resource: bid_resource.to_string(),
            min_bid,
            ends_at,
            high_bidder: None,
            high_bid: 0,
            status: SettlementStatus::Open,
        },
    );
    mark_receipt(world, SettlementKind::Auction, id, SettlementPhase::Reserve);
    record_account_flow(
        world,
        player_account(seller),
        reserve_account(SettlementKind::Auction, id, "lot"),
        lot_resource,
        lot_amount,
        ResourceOperation::plugin_settlement(PluginSettlementKind::Auction),
    );
    Ok(())
}

fn validate_bid_auction(
    world: &World,
    bidder: PlayerId,
    id: SettlementId,
    bid: u32,
) -> CommandResult {
    ensure_positive(bid)?;
    let auction = world
        .resource::<SettlementState>()
        .auctions
        .get(&id)
        .ok_or(RejectionReason::OrderNotFound)?;
    ensure_open(auction.status)?;
    if bidder == auction.seller
        || world.resource::<CurrentTick>().0 >= auction.ends_at
        || bid < auction.min_bid
        || bid <= auction.high_bid
    {
        return Err(RejectionReason::OrderNotFound);
    }
    ensure_player_global_resource(world, bidder, &auction.bid_resource, bid)
}

fn apply_bid_auction(
    world: &mut World,
    bidder: PlayerId,
    id: SettlementId,
    bid: u32,
) -> CommandResult {
    validate_bid_auction(world, bidder, id, bid)?;
    let prior = world
        .resource::<SettlementState>()
        .auctions
        .get(&id)
        .cloned()
        .ok_or(RejectionReason::OrderNotFound)?;
    if let Some(prior_bidder) = prior.high_bidder {
        release_reserved_to_player(
            world,
            SettlementKind::Auction,
            id,
            prior_bidder,
            &prior.bid_resource,
            prior.high_bid,
        )?;
        record_account_flow(
            world,
            reserve_account(SettlementKind::Auction, id, "bid"),
            player_account(prior_bidder),
            &prior.bid_resource,
            prior.high_bid,
            ResourceOperation::plugin_settlement(PluginSettlementKind::Auction),
        );
    }
    reserve_player_resource(
        world,
        SettlementKind::Auction,
        id,
        bidder,
        &prior.bid_resource,
        bid,
    )?;
    let mut state = world.resource_mut::<SettlementState>();
    let auction = state.auctions.get_mut(&id).unwrap();
    auction.high_bidder = Some(bidder);
    auction.high_bid = bid;
    mark_receipt(world, SettlementKind::Auction, id, SettlementPhase::Accept);
    record_account_flow(
        world,
        player_account(bidder),
        reserve_account(SettlementKind::Auction, id, "bid"),
        &prior.bid_resource,
        bid,
        ResourceOperation::plugin_settlement(PluginSettlementKind::Auction),
    );
    Ok(())
}

fn validate_settle_auction(world: &World, id: SettlementId) -> CommandResult {
    if has_receipt(world, SettlementKind::Auction, id, SettlementPhase::Settle) {
        return Ok(());
    }
    let auction = world
        .resource::<SettlementState>()
        .auctions
        .get(&id)
        .ok_or(RejectionReason::OrderNotFound)?;
    ensure_open(auction.status)?;
    if world.resource::<CurrentTick>().0 < auction.ends_at || auction.high_bidder.is_none() {
        return Err(RejectionReason::OrderNotFound);
    }
    Ok(())
}
fn apply_settle_auction(world: &mut World, id: SettlementId) -> CommandResult {
    validate_settle_auction(world, id)?;
    if has_receipt(world, SettlementKind::Auction, id, SettlementPhase::Settle) {
        return Ok(());
    }
    let auction = world
        .resource::<SettlementState>()
        .auctions
        .get(&id)
        .cloned()
        .ok_or(RejectionReason::OrderNotFound)?;
    let winner = auction.high_bidder.ok_or(RejectionReason::OrderNotFound)?;
    release_reserved_to_player(
        world,
        SettlementKind::Auction,
        id,
        winner,
        &auction.lot_resource,
        auction.lot_amount,
    )?;
    release_reserved_to_player(
        world,
        SettlementKind::Auction,
        id,
        auction.seller,
        &auction.bid_resource,
        auction.high_bid,
    )?;
    world
        .resource_mut::<SettlementState>()
        .auctions
        .get_mut(&id)
        .unwrap()
        .status = SettlementStatus::Settled;
    mark_receipt(world, SettlementKind::Auction, id, SettlementPhase::Settle);
    record_account_flow(
        world,
        reserve_account(SettlementKind::Auction, id, "lot"),
        player_account(winner),
        &auction.lot_resource,
        auction.lot_amount,
        ResourceOperation::plugin_settlement(PluginSettlementKind::Auction),
    );
    record_account_flow(
        world,
        reserve_account(SettlementKind::Auction, id, "bid"),
        player_account(auction.seller),
        &auction.bid_resource,
        auction.high_bid,
        ResourceOperation::plugin_settlement(PluginSettlementKind::Auction),
    );
    Ok(())
}
fn validate_cancel_auction(world: &World, seller: PlayerId, id: SettlementId) -> CommandResult {
    let auction = world
        .resource::<SettlementState>()
        .auctions
        .get(&id)
        .ok_or(RejectionReason::OrderNotFound)?;
    if auction.seller != seller || auction.high_bidder.is_some() {
        return Err(RejectionReason::NotOwner);
    }
    ensure_open(auction.status)
}
fn apply_cancel_auction(world: &mut World, seller: PlayerId, id: SettlementId) -> CommandResult {
    validate_cancel_auction(world, seller, id)?;
    if has_receipt(world, SettlementKind::Auction, id, SettlementPhase::Cancel) {
        return Ok(());
    }
    let auction = world
        .resource::<SettlementState>()
        .auctions
        .get(&id)
        .cloned()
        .ok_or(RejectionReason::OrderNotFound)?;
    release_reserved_to_player(
        world,
        SettlementKind::Auction,
        id,
        auction.seller,
        &auction.lot_resource,
        auction.lot_amount,
    )?;
    world
        .resource_mut::<SettlementState>()
        .auctions
        .get_mut(&id)
        .unwrap()
        .status = SettlementStatus::Cancelled;
    mark_receipt(world, SettlementKind::Auction, id, SettlementPhase::Cancel);
    record_account_flow(
        world,
        reserve_account(SettlementKind::Auction, id, "lot"),
        player_account(auction.seller),
        &auction.lot_resource,
        auction.lot_amount,
        ResourceOperation::plugin_settlement(PluginSettlementKind::Auction),
    );
    Ok(())
}

fn validate_create_escrow(
    world: &World,
    payer: PlayerId,
    id: SettlementId,
    nonce: u64,
    payee: PlayerId,
    arbiter: PlayerId,
    resource: &str,
    amount: u32,
) -> CommandResult {
    ensure_positive(amount)?;
    ensure_settlement_id_available(world, id, payer, nonce)?;
    if payer == payee {
        return Err(RejectionReason::NotFriendly);
    }
    let _ = arbiter;
    ensure_player_global_resource(world, payer, resource, amount)
}
fn apply_create_escrow(
    world: &mut World,
    payer: PlayerId,
    id: SettlementId,
    nonce: u64,
    payee: PlayerId,
    arbiter: PlayerId,
    resource: &str,
    amount: u32,
) -> CommandResult {
    validate_create_escrow(world, payer, id, nonce, payee, arbiter, resource, amount)?;
    reserve_player_resource(world, SettlementKind::Escrow, id, payer, resource, amount)?;
    world.resource_mut::<SettlementState>().id_nonces.insert(
        id,
        SettlementIdNonce {
            owner: payer,
            nonce,
        },
    );
    world.resource_mut::<SettlementState>().escrows.insert(
        id,
        EscrowSettlement {
            payer,
            payee,
            arbiter,
            resource: resource.to_string(),
            amount,
            status: SettlementStatus::Open,
        },
    );
    mark_receipt(world, SettlementKind::Escrow, id, SettlementPhase::Reserve);
    record_account_flow(
        world,
        player_account(payer),
        reserve_account(SettlementKind::Escrow, id, "escrow"),
        resource,
        amount,
        settlement_operation(PluginSettlementKind::Escrow),
    );
    Ok(())
}
fn validate_release_escrow(world: &World, actor: PlayerId, id: SettlementId) -> CommandResult {
    let escrow = world
        .resource::<SettlementState>()
        .escrows
        .get(&id)
        .ok_or(RejectionReason::OrderNotFound)?;
    if actor != escrow.payer && actor != escrow.arbiter {
        return Err(RejectionReason::NotOwner);
    }
    if has_receipt(world, SettlementKind::Escrow, id, SettlementPhase::Settle) {
        return Ok(());
    }
    ensure_open(escrow.status)?;
    Ok(())
}
fn apply_release_escrow(world: &mut World, actor: PlayerId, id: SettlementId) -> CommandResult {
    validate_release_escrow(world, actor, id)?;
    if has_receipt(world, SettlementKind::Escrow, id, SettlementPhase::Settle) {
        return Ok(());
    }
    let escrow = world
        .resource::<SettlementState>()
        .escrows
        .get(&id)
        .cloned()
        .ok_or(RejectionReason::OrderNotFound)?;
    release_reserved_to_player(
        world,
        SettlementKind::Escrow,
        id,
        escrow.payee,
        &escrow.resource,
        escrow.amount,
    )?;
    world
        .resource_mut::<SettlementState>()
        .escrows
        .get_mut(&id)
        .unwrap()
        .status = SettlementStatus::Settled;
    mark_receipt(world, SettlementKind::Escrow, id, SettlementPhase::Settle);
    record_account_flow(
        world,
        reserve_account(SettlementKind::Escrow, id, "escrow"),
        player_account(escrow.payee),
        &escrow.resource,
        escrow.amount,
        settlement_operation(PluginSettlementKind::Escrow),
    );
    Ok(())
}
fn validate_refund_escrow(world: &World, actor: PlayerId, id: SettlementId) -> CommandResult {
    let escrow = world
        .resource::<SettlementState>()
        .escrows
        .get(&id)
        .ok_or(RejectionReason::OrderNotFound)?;
    if actor != escrow.payee && actor != escrow.arbiter {
        return Err(RejectionReason::NotOwner);
    }
    if has_receipt(world, SettlementKind::Escrow, id, SettlementPhase::Refund) {
        return Ok(());
    }
    ensure_open(escrow.status)?;
    Ok(())
}
fn apply_refund_escrow(world: &mut World, actor: PlayerId, id: SettlementId) -> CommandResult {
    validate_refund_escrow(world, actor, id)?;
    if has_receipt(world, SettlementKind::Escrow, id, SettlementPhase::Refund) {
        return Ok(());
    }
    let escrow = world
        .resource::<SettlementState>()
        .escrows
        .get(&id)
        .cloned()
        .ok_or(RejectionReason::OrderNotFound)?;
    release_reserved_to_player(
        world,
        SettlementKind::Escrow,
        id,
        escrow.payer,
        &escrow.resource,
        escrow.amount,
    )?;
    world
        .resource_mut::<SettlementState>()
        .escrows
        .get_mut(&id)
        .unwrap()
        .status = SettlementStatus::Refunded;
    mark_receipt(world, SettlementKind::Escrow, id, SettlementPhase::Refund);
    record_account_flow(
        world,
        reserve_account(SettlementKind::Escrow, id, "escrow"),
        player_account(escrow.payer),
        &escrow.resource,
        escrow.amount,
        settlement_operation(PluginSettlementKind::Escrow),
    );
    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn validate_create_loan_offer(
    world: &World,
    lender: PlayerId,
    id: SettlementId,
    nonce: u64,
    borrower: PlayerId,
    resource: &str,
    principal: u32,
    repay_amount: u32,
    due_at: Tick,
) -> CommandResult {
    ensure_positive(principal)?;
    ensure_positive(repay_amount)?;
    if borrower == lender || repay_amount < principal || world.resource::<CurrentTick>().0 >= due_at
    {
        return Err(RejectionReason::OrderNotFound);
    }
    ensure_settlement_id_available(world, id, lender, nonce)?;
    ensure_player_global_resource(world, lender, resource, principal)
}
#[allow(clippy::too_many_arguments)]
fn apply_create_loan_offer(
    world: &mut World,
    lender: PlayerId,
    id: SettlementId,
    nonce: u64,
    borrower: PlayerId,
    resource: &str,
    principal: u32,
    repay_amount: u32,
    due_at: Tick,
) -> CommandResult {
    validate_create_loan_offer(
        world,
        lender,
        id,
        nonce,
        borrower,
        resource,
        principal,
        repay_amount,
        due_at,
    )?;
    reserve_player_resource(
        world,
        SettlementKind::Lending,
        id,
        lender,
        resource,
        principal,
    )?;
    world.resource_mut::<SettlementState>().id_nonces.insert(
        id,
        SettlementIdNonce {
            owner: lender,
            nonce,
        },
    );
    world.resource_mut::<SettlementState>().loans.insert(
        id,
        LendingSettlement {
            lender,
            borrower,
            resource: resource.to_string(),
            principal,
            repay_amount,
            due_at,
            accepted: false,
            status: SettlementStatus::Open,
        },
    );
    mark_receipt(world, SettlementKind::Lending, id, SettlementPhase::Reserve);
    record_account_flow(
        world,
        player_account(lender),
        reserve_account(SettlementKind::Lending, id, "principal"),
        resource,
        principal,
        ResourceOperation::plugin_settlement(PluginSettlementKind::Lending),
    );
    Ok(())
}
fn validate_accept_loan(world: &World, borrower: PlayerId, id: SettlementId) -> CommandResult {
    let loan = world
        .resource::<SettlementState>()
        .loans
        .get(&id)
        .ok_or(RejectionReason::OrderNotFound)?;
    if loan.borrower != borrower {
        return Err(RejectionReason::OrderNotFound);
    }
    if has_receipt(world, SettlementKind::Lending, id, SettlementPhase::Accept) {
        return Ok(());
    }
    ensure_open(loan.status)?;
    if loan.accepted || world.resource::<CurrentTick>().0 >= loan.due_at {
        return Err(RejectionReason::OrderNotFound);
    }
    Ok(())
}
fn apply_accept_loan(world: &mut World, borrower: PlayerId, id: SettlementId) -> CommandResult {
    validate_accept_loan(world, borrower, id)?;
    if has_receipt(world, SettlementKind::Lending, id, SettlementPhase::Accept) {
        return Ok(());
    }
    let loan = world
        .resource::<SettlementState>()
        .loans
        .get(&id)
        .cloned()
        .ok_or(RejectionReason::OrderNotFound)?;
    release_reserved_to_player(
        world,
        SettlementKind::Lending,
        id,
        borrower,
        &loan.resource,
        loan.principal,
    )?;
    let mut state = world.resource_mut::<SettlementState>();
    let loan_mut = state.loans.get_mut(&id).unwrap();
    loan_mut.accepted = true;
    loan_mut.status = SettlementStatus::Accepted;
    mark_receipt(world, SettlementKind::Lending, id, SettlementPhase::Accept);
    record_account_flow(
        world,
        reserve_account(SettlementKind::Lending, id, "principal"),
        player_account(borrower),
        &loan.resource,
        loan.principal,
        ResourceOperation::plugin_settlement(PluginSettlementKind::Lending),
    );
    Ok(())
}
fn validate_repay_loan(world: &World, borrower: PlayerId, id: SettlementId) -> CommandResult {
    let loan = world
        .resource::<SettlementState>()
        .loans
        .get(&id)
        .ok_or(RejectionReason::OrderNotFound)?;
    if loan.borrower != borrower {
        return Err(RejectionReason::OrderNotFound);
    }
    if has_receipt(world, SettlementKind::Lending, id, SettlementPhase::Repay) {
        return Ok(());
    }
    if !loan.accepted || loan.status.is_terminal() {
        return Err(RejectionReason::OrderNotFound);
    }
    ensure_player_global_resource(world, borrower, &loan.resource, loan.repay_amount)
}
fn apply_repay_loan(world: &mut World, borrower: PlayerId, id: SettlementId) -> CommandResult {
    validate_repay_loan(world, borrower, id)?;
    if has_receipt(world, SettlementKind::Lending, id, SettlementPhase::Repay) {
        return Ok(());
    }
    let loan = world
        .resource::<SettlementState>()
        .loans
        .get(&id)
        .cloned()
        .ok_or(RejectionReason::OrderNotFound)?;
    debit_global(world, borrower, &loan.resource, loan.repay_amount)?;
    credit_global(world, loan.lender, &loan.resource, loan.repay_amount);
    world
        .resource_mut::<SettlementState>()
        .loans
        .get_mut(&id)
        .unwrap()
        .status = SettlementStatus::Repaid;
    mark_receipt(world, SettlementKind::Lending, id, SettlementPhase::Repay);
    record_account_flow(
        world,
        player_account(borrower),
        player_account(loan.lender),
        &loan.resource,
        loan.repay_amount,
        ResourceOperation::plugin_settlement(PluginSettlementKind::Lending),
    );
    Ok(())
}
fn validate_default_loan(world: &World, id: SettlementId) -> CommandResult {
    if has_receipt(world, SettlementKind::Lending, id, SettlementPhase::Default) {
        return Ok(());
    }
    let loan = world
        .resource::<SettlementState>()
        .loans
        .get(&id)
        .ok_or(RejectionReason::OrderNotFound)?;
    if !loan.accepted
        || loan.status.is_terminal()
        || world.resource::<CurrentTick>().0 <= loan.due_at
    {
        return Err(RejectionReason::OrderNotFound);
    }
    Ok(())
}
fn apply_default_loan(world: &mut World, id: SettlementId) -> CommandResult {
    validate_default_loan(world, id)?;
    if has_receipt(world, SettlementKind::Lending, id, SettlementPhase::Default) {
        return Ok(());
    }
    let loan = world
        .resource::<SettlementState>()
        .loans
        .get(&id)
        .cloned()
        .ok_or(RejectionReason::OrderNotFound)?;
    world
        .resource_mut::<SettlementState>()
        .loans
        .get_mut(&id)
        .unwrap()
        .status = SettlementStatus::Defaulted;
    mark_receipt(world, SettlementKind::Lending, id, SettlementPhase::Default);
    let _ = loan;
    Ok(())
}

fn apply_transfer_to_global(
    world: &mut World,
    player_id: PlayerId,
    resource: &str,
    amount: u32,
) -> CommandResult {
    let config = world.resource::<GlobalStorageConfig>().clone();
    let fee = transfer_fee(amount, config.transfer_to_global_fee_per_10_000);
    let deliver_amount = amount.saturating_sub(fee);
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
            deliver_amount,
            remaining_ticks: config.transfer_to_global_ticks,
            start: player_storage_position(player_id),
            end: global_storage_position(player_id),
        });
    record_resource_flow(
        world,
        Some(player_id),
        Some(player_id),
        resource,
        amount,
        ResourceOperation::GlobalDeposit,
        fee,
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
    let fee = transfer_fee(amount, config.transfer_from_global_fee_per_10_000);
    let deliver_amount = amount.saturating_sub(fee);
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
            deliver_amount,
            remaining_ticks: config.transfer_from_global_ticks,
            start: global_storage_position(player_id),
            end: player_storage_position(player_id),
        });
    record_resource_flow(
        world,
        Some(player_id),
        Some(player_id),
        resource,
        amount,
        ResourceOperation::GlobalWithdraw,
        fee,
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
    if let Some(resource_store) = entity_ref.get::<Resource>() {
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
            return Err(RejectionReason::CapacityExceeded);
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
        return Err(RejectionReason::CapacityExceeded);
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
        return Err(RejectionReason::CooldownActive);
    }
    if check_fatigue && drone.fatigue > 0 {
        return Err(RejectionReason::CooldownActive);
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
        let _ = part;
        return Err(RejectionReason::InsufficientResources);
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
            .query::<(&Position, &Resource)>()
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

fn record_account_flow(
    world: &mut World,
    source: LedgerAccount,
    target: LedgerAccount,
    resource: &str,
    amount: u32,
    operation: ResourceOperation,
) {
    if amount == 0 {
        return;
    }
    let tick = world.resource::<CurrentTick>().0;
    world
        .resource_mut::<ResourceLedger>()
        .record_account_transfer(tick, source, target, resource, amount, operation);
}

fn player_account(player: PlayerId) -> LedgerAccount {
    LedgerAccount::player(player)
}

fn reserve_account(kind: SettlementKind, id: SettlementId, label: &str) -> LedgerAccount {
    LedgerAccount::reserve(kind, id, label)
}

fn merchant_account(quote_id: SettlementId) -> LedgerAccount {
    LedgerAccount::merchant(quote_id)
}

fn settlement_sink(label: &str) -> LedgerAccount {
    LedgerAccount::sink(label)
}

fn settlement_system(label: &str) -> LedgerAccount {
    LedgerAccount::system(label)
}

fn settlement_operation(kind: PluginSettlementKind) -> ResourceOperation {
    ResourceOperation::plugin_settlement(kind)
}

fn ensure_positive(amount: u32) -> CommandResult {
    if amount == 0 {
        Err(RejectionReason::InsufficientResources)
    } else {
        Ok(())
    }
}

fn ensure_valid_resource_names(left: &str, right: &str) -> CommandResult {
    if left.is_empty() || right.is_empty() {
        Err(RejectionReason::InvalidResourceType)
    } else {
        Ok(())
    }
}

fn ensure_open(status: SettlementStatus) -> CommandResult {
    if status.is_terminal() {
        Err(RejectionReason::OrderNotFound)
    } else {
        Ok(())
    }
}

fn ensure_not_expired(world: &World, expires_at: Option<Tick>) -> CommandResult {
    if let Some(expires_at) = expires_at
        && world.resource::<CurrentTick>().0 > expires_at
    {
        return Err(RejectionReason::OrderNotFound);
    }
    Ok(())
}

fn ensure_settlement_id_available(
    world: &World,
    id: SettlementId,
    owner: PlayerId,
    nonce: u64,
) -> CommandResult {
    if id == 0 {
        return Err(RejectionReason::AuthContextInvalid);
    }
    let state = world.resource::<SettlementState>();
    if state.id_nonces.contains_key(&id)
        || state.contracts.contains_key(&id)
        || state.merchant_quotes.contains_key(&id)
        || state.p2p_offers.contains_key(&id)
        || state.auctions.contains_key(&id)
        || state.escrows.contains_key(&id)
        || state.loans.contains_key(&id)
    {
        return Err(RejectionReason::OrderNotFound);
    }
    if deterministic_settlement_id(owner, nonce) != id {
        return Err(RejectionReason::AuthContextInvalid);
    }
    Ok(())
}

fn deterministic_settlement_id(owner: PlayerId, nonce: u64) -> SettlementId {
    let mut hasher = blake3::Hasher::new();
    hasher.update(b"swarm:settlement-id:v1");
    hasher.update(&owner.to_le_bytes());
    hasher.update(&nonce.to_le_bytes());
    u64::from_le_bytes(
        hasher.finalize().as_bytes()[..8]
            .try_into()
            .expect("BLAKE3 digest has 32 bytes"),
    )
}

fn receipt(kind: SettlementKind, id: SettlementId, phase: SettlementPhase) -> SettlementKey {
    SettlementKey { kind, id, phase }
}

fn has_receipt(
    world: &World,
    kind: SettlementKind,
    id: SettlementId,
    phase: SettlementPhase,
) -> bool {
    world
        .resource::<SettlementState>()
        .receipts
        .contains_key(&receipt(kind, id, phase))
}

fn mark_receipt(world: &mut World, kind: SettlementKind, id: SettlementId, phase: SettlementPhase) {
    world
        .resource_mut::<SettlementState>()
        .receipts
        .insert(receipt(kind, id, phase), true);
}

fn ensure_player_global_resource(
    world: &World,
    player: PlayerId,
    resource: &str,
    amount: u32,
) -> CommandResult {
    let available = world
        .resource::<PlayerGlobalStorage>()
        .0
        .get(&player)
        .and_then(|storage| storage.get(resource))
        .copied()
        .unwrap_or_default();
    if available < amount {
        Err(RejectionReason::InsufficientResource {
            resource: resource.to_string(),
            required: amount,
            available,
        })
    } else {
        Ok(())
    }
}

fn debit_global(world: &mut World, player: PlayerId, resource: &str, amount: u32) -> CommandResult {
    ensure_player_global_resource(world, player, resource, amount)?;
    subtract_player_resource(
        world
            .resource_mut::<PlayerGlobalStorage>()
            .0
            .entry(player)
            .or_default(),
        resource,
        amount,
    );
    Ok(())
}

fn credit_global(world: &mut World, player: PlayerId, resource: &str, amount: u32) {
    *world
        .resource_mut::<PlayerGlobalStorage>()
        .0
        .entry(player)
        .or_default()
        .entry(resource.to_string())
        .or_default() += amount;
}

fn reserve_player_resource(
    world: &mut World,
    kind: SettlementKind,
    id: SettlementId,
    player: PlayerId,
    resource: &str,
    amount: u32,
) -> CommandResult {
    debit_global(world, player, resource, amount)?;
    *world
        .resource_mut::<SettlementState>()
        .reserves
        .entry((kind, id))
        .or_default()
        .entry(resource.to_string())
        .or_default() += amount;
    Ok(())
}

fn release_reserved_to_player(
    world: &mut World,
    kind: SettlementKind,
    id: SettlementId,
    player: PlayerId,
    resource: &str,
    amount: u32,
) -> CommandResult {
    consume_reserved(world, kind, id, resource, amount)?;
    credit_global(world, player, resource, amount);
    Ok(())
}

fn consume_reserved(
    world: &mut World,
    kind: SettlementKind,
    id: SettlementId,
    resource: &str,
    amount: u32,
) -> CommandResult {
    let mut state = world.resource_mut::<SettlementState>();
    let reserve = state.reserves.entry((kind, id)).or_default();
    let available = reserve.get(resource).copied().unwrap_or_default();
    if available < amount {
        return Err(RejectionReason::InsufficientResource {
            resource: resource.to_string(),
            required: amount,
            available,
        });
    }
    reserve.insert(resource.to_string(), available - amount);
    Ok(())
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
    if let Some(mut resource_store) = world.entity_mut(entity).get_mut::<Resource>() {
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

    fn run_settlement_action(
        swarm: &mut crate::SwarmWorld,
        player_id: PlayerId,
        tick: Tick,
        source: CommandSource,
        action: CommandAction,
    ) -> CommandResult {
        let world = swarm.app.world_mut();
        let raw = RawCommand {
            player_id,
            tick,
            source,
            auth: CommandAuth::server_injected(source, player_id, tick, tick),
            sequence: 1,
            action,
        };
        let validated = validate_command(world, raw)?;
        apply_command(world, validated)
    }

    fn set_global(swarm: &mut crate::SwarmWorld, player: PlayerId, resource: &str, amount: u32) {
        swarm
            .app
            .world_mut()
            .resource_mut::<PlayerGlobalStorage>()
            .0
            .entry(player)
            .or_default()
            .insert(resource.to_string(), amount);
    }

    fn global_amount(swarm: &crate::SwarmWorld, player: PlayerId, resource: &str) -> u32 {
        swarm
            .app
            .world()
            .resource::<PlayerGlobalStorage>()
            .0
            .get(&player)
            .and_then(|storage| storage.get(resource))
            .copied()
            .unwrap_or_default()
    }

    fn local_amount(swarm: &crate::SwarmWorld, player: PlayerId, resource: &str) -> u32 {
        swarm
            .app
            .world()
            .resource::<PlayerLocalStorage>()
            .0
            .get(&player)
            .and_then(|storage| storage.get(resource))
            .copied()
            .unwrap_or_default()
    }

    fn submit_transfer_action(
        world: &mut crate::SwarmWorld,
        player_id: PlayerId,
        action: CommandAction,
    ) {
        world
            .submit_raw_command(RawCommand {
                player_id,
                tick: 1,
                source: CommandSource::TestHarness,
                auth: CommandAuth::server_injected(CommandSource::TestHarness, player_id, 1, 1),
                sequence: 1,
                action,
            })
            .unwrap();
    }

    fn seed_first_spawn(world: &mut crate::SwarmWorld, player_id: PlayerId) {
        world
            .app
            .world_mut()
            .resource_mut::<crate::systems::PlayerFirstSpawnTick>()
            .0
            .insert(player_id, 1);
    }

    fn settlement_id(owner: PlayerId, nonce: u64) -> SettlementId {
        deterministic_settlement_id(owner, nonce)
    }

    fn economy_action(action_type: &str, payload: serde_json::Value) -> CommandAction {
        CommandAction::Action {
            action_type: action_type.to_string(),
            object_id: 0,
            target_id: None,
            payload,
        }
    }

    fn wire_action(value: serde_json::Value) -> CommandAction {
        serde_json::from_value(value).unwrap()
    }

    #[test]
    fn contract_settlement_reserves_settles_once_and_is_idempotent() {
        let mut world = create_world();
        set_global(&mut world, 1, "Energy", 100);
        let id = settlement_id(1, 7);

        run_settlement_action(
            &mut world,
            1,
            1,
            CommandSource::TestHarness,
            economy_action(
                "CreateContractSettlement",
                serde_json::json!({
                    "settlement_id": id,
                    "nonce": 7,
                    "input_resource": "Energy",
                    "input_amount": 40,
                    "output_resource": "Energy",
                    "output_amount": 35,
                    "expires_at": 10,
                }),
            ),
        )
        .unwrap();
        assert_eq!(global_amount(&world, 1, "Energy"), 60);

        run_settlement_action(
            &mut world,
            1,
            1,
            CommandSource::TestHarness,
            economy_action("SettleContract", serde_json::json!({ "settlement_id": id })),
        )
        .unwrap();
        run_settlement_action(
            &mut world,
            1,
            1,
            CommandSource::TestHarness,
            economy_action("SettleContract", serde_json::json!({ "settlement_id": id })),
        )
        .unwrap();

        assert_eq!(global_amount(&world, 1, "Energy"), 95);
        assert!(
            world
                .app
                .world()
                .resource::<SettlementState>()
                .receipts
                .contains_key(&receipt(
                    SettlementKind::Contract,
                    id,
                    SettlementPhase::Settle
                ))
        );
        let ledger = world.app.world().resource::<ResourceLedger>();
        let contract_ops = ledger
            .ops
            .iter()
            .filter(|op| {
                op.operation == ResourceOperation::plugin_settlement(PluginSettlementKind::Contract)
            })
            .collect::<Vec<_>>();
        assert_eq!(contract_ops.len(), 3);
        assert!(contract_ops.iter().any(|op| {
            matches!(
                (&op.source_account, &op.target_account),
                (
                    Some(LedgerAccount::Player { player_id: 1 }),
                    Some(LedgerAccount::Reserve { kind: SettlementKind::Contract, id: reserve_id, label })
                ) if *reserve_id == id && label == "input"
            )
        }));
        assert!(contract_ops.iter().any(|op| {
            matches!(
                (&op.source_account, &op.target_account),
                (
                    Some(LedgerAccount::Reserve { kind: SettlementKind::Contract, id: reserve_id, label }),
                    Some(LedgerAccount::Sink { label: sink_label })
                ) if *reserve_id == id && label == "input" && sink_label == "contract_remainder"
            )
        }));
    }

    #[test]
    fn merchant_trade_requires_server_quote_and_min_receive() {
        let mut world = create_world();
        set_global(&mut world, 1, "Energy", 50);
        let forged = run_settlement_action(
            &mut world,
            1,
            1,
            CommandSource::TestHarness,
            wire_action(serde_json::json!({
                "type": "CreateMerchantQuote",
                "quote_id": 9,
                "player_id": 1,
                "pay_resource": "Energy",
                "pay_amount": 10,
                "receive_resource": "Ore",
                "receive_amount": 9,
                "expires_at": 10,
            })),
        );
        assert_eq!(forged, Err(RejectionReason::SourceNotAllowed));

        run_settlement_action(
            &mut world,
            0,
            1,
            CommandSource::TestHarness,
            wire_action(serde_json::json!({
                "type": "CreateMerchantQuote",
                "quote_id": 9,
                "player_id": 1,
                "pay_resource": "Energy",
                "pay_amount": 10,
                "receive_resource": "Ore",
                "receive_amount": 9,
                "expires_at": 10,
            })),
        )
        .unwrap();
        assert!(
            run_settlement_action(
                &mut world,
                1,
                1,
                CommandSource::TestHarness,
                wire_action(serde_json::json!({
                    "type": "AcceptMerchantTrade",
                    "quote_id": 9,
                    "min_receive": 10,
                })),
            )
            .is_err()
        );
        run_settlement_action(
            &mut world,
            1,
            1,
            CommandSource::TestHarness,
            wire_action(serde_json::json!({
                "type": "AcceptMerchantTrade",
                "quote_id": 9,
                "min_receive": 9,
            })),
        )
        .unwrap();
        assert_eq!(global_amount(&world, 1, "Energy"), 40);
        assert_eq!(global_amount(&world, 1, "Ore"), 9);

        let ledger = world
            .app
            .world()
            .resource::<ResourceLedger>()
            .trace_snapshot();
        assert!(ledger.conservation_imbalance.is_empty());
        assert!(ledger.operations.iter().any(|op| {
            matches!(
                (&op.source_account, &op.target_account),
                (
                    Some(LedgerAccount::Player { player_id: 1 }),
                    Some(LedgerAccount::Merchant { quote_id })
                ) if *quote_id == 9 && op.resource == "Energy" && op.amount_requested == 10
            )
        }));
        assert!(ledger.operations.iter().any(|op| {
            matches!(
                (&op.source_account, &op.target_account),
                (
                    Some(LedgerAccount::Merchant { quote_id }),
                    Some(LedgerAccount::Player { player_id: 1 })
                ) if *quote_id == 9 && op.resource == "Ore" && op.amount_requested == 9
            )
        }));
    }

    #[test]
    fn settlement_ids_are_domain_separated_and_zero_is_rejected() {
        let old_owner_one = (1_u64 << 32) ^ 0;
        let old_owner_two = (2_u64 << 32) ^ (3_u64 << 32);
        assert_eq!(old_owner_one, old_owner_two);
        assert_ne!(settlement_id(1, 0), settlement_id(2, 3_u64 << 32));

        let mut world = create_world();
        set_global(&mut world, 1, "Energy", 10);
        assert_eq!(
            run_settlement_action(
                &mut world,
                1,
                1,
                CommandSource::TestHarness,
                wire_action(serde_json::json!({
                    "type": "CreateEscrow",
                    "escrow_id": 0,
                    "nonce": 1,
                    "payee": 2,
                    "arbiter": 3,
                    "resource": "Energy",
                    "amount": 1,
                })),
            ),
            Err(RejectionReason::AuthContextInvalid)
        );
    }

    #[test]
    fn terminal_receipts_authorize_actor_before_idempotency() {
        let mut world = create_world();
        set_global(&mut world, 1, "Energy", 100);
        let contract_id = settlement_id(1, 31);
        run_settlement_action(
            &mut world,
            1,
            1,
            CommandSource::TestHarness,
            wire_action(serde_json::json!({
                "type": "CreateContractSettlement",
                "settlement_id": contract_id,
                "nonce": 31,
                "input_resource": "Energy",
                "input_amount": 10,
                "output_resource": "Energy",
                "output_amount": 10,
                "expires_at": 10,
            })),
        )
        .unwrap();
        run_settlement_action(
            &mut world,
            1,
            1,
            CommandSource::TestHarness,
            wire_action(serde_json::json!({
                "type": "SettleContract",
                "settlement_id": contract_id,
            })),
        )
        .unwrap();
        assert_eq!(
            run_settlement_action(
                &mut world,
                2,
                1,
                CommandSource::TestHarness,
                wire_action(serde_json::json!({
                    "type": "SettleContract",
                    "settlement_id": contract_id,
                })),
            ),
            Err(RejectionReason::NotOwner)
        );

        let escrow_id = settlement_id(1, 32);
        run_settlement_action(
            &mut world,
            1,
            1,
            CommandSource::TestHarness,
            wire_action(serde_json::json!({
                "type": "CreateEscrow",
                "escrow_id": escrow_id,
                "nonce": 32,
                "payee": 2,
                "arbiter": 3,
                "resource": "Energy",
                "amount": 10,
            })),
        )
        .unwrap();
        run_settlement_action(
            &mut world,
            3,
            1,
            CommandSource::TestHarness,
            wire_action(serde_json::json!({
                "type": "ReleaseEscrow",
                "escrow_id": escrow_id,
            })),
        )
        .unwrap();
        assert!(
            run_settlement_action(
                &mut world,
                3,
                1,
                CommandSource::TestHarness,
                wire_action(serde_json::json!({
                    "type": "RefundEscrow",
                    "escrow_id": escrow_id,
                })),
            )
            .is_err()
        );
    }

    #[test]
    fn lending_default_is_status_only_and_does_not_mint() {
        let mut world = create_world();
        set_global(&mut world, 1, "Energy", 50);
        let loan_id = settlement_id(1, 33);
        run_settlement_action(
            &mut world,
            1,
            1,
            CommandSource::TestHarness,
            wire_action(serde_json::json!({
                "type": "CreateLoanOffer",
                "loan_id": loan_id,
                "nonce": 33,
                "borrower": 2,
                "resource": "Energy",
                "principal": 10,
                "repay_amount": 12,
                "due_at": 4,
            })),
        )
        .unwrap();
        run_settlement_action(
            &mut world,
            2,
            1,
            CommandSource::TestHarness,
            wire_action(serde_json::json!({
                "type": "AcceptLoan",
                "loan_id": loan_id,
            })),
        )
        .unwrap();
        let total_before = global_amount(&world, 1, "Energy") + global_amount(&world, 2, "Energy");
        let ops_before = world
            .app
            .world()
            .resource::<ResourceLedger>()
            .ops
            .iter()
            .filter(|op| {
                op.operation == ResourceOperation::plugin_settlement(PluginSettlementKind::Lending)
            })
            .count();
        world.app.world_mut().resource_mut::<CurrentTick>().0 = 5;
        run_settlement_action(
            &mut world,
            99,
            5,
            CommandSource::TestHarness,
            wire_action(serde_json::json!({
                "type": "DefaultLoan",
                "loan_id": loan_id,
            })),
        )
        .unwrap();
        assert_eq!(
            world
                .app
                .world()
                .resource::<SettlementState>()
                .loans
                .get(&loan_id)
                .unwrap()
                .status,
            SettlementStatus::Defaulted
        );
        assert_eq!(
            global_amount(&world, 1, "Energy") + global_amount(&world, 2, "Energy"),
            total_before
        );
        assert_eq!(
            world
                .app
                .world()
                .resource::<ResourceLedger>()
                .ops
                .iter()
                .filter(|op| {
                    op.operation
                        == ResourceOperation::plugin_settlement(PluginSettlementKind::Lending)
                })
                .count(),
            ops_before
        );
    }

    #[test]
    fn p2p_offer_auction_escrow_and_lending_workflows_conserve_balances() {
        let mut world = create_world();
        set_global(&mut world, 1, "Energy", 200);
        set_global(&mut world, 2, "Ore", 100);
        set_global(&mut world, 2, "Energy", 100);
        set_global(&mut world, 3, "Ore", 50);

        let offer_id = settlement_id(1, 11);
        run_settlement_action(
            &mut world,
            1,
            1,
            CommandSource::TestHarness,
            wire_action(serde_json::json!({
                "type": "CreateP2POffer",
                "offer_id": offer_id,
                "nonce": 11,
                "give_resource": "Energy",
                "give_amount": 30,
                "want_resource": "Ore",
                "want_amount": 10,
                "expires_at": 10,
            })),
        )
        .unwrap();
        run_settlement_action(
            &mut world,
            2,
            1,
            CommandSource::TestHarness,
            wire_action(serde_json::json!({ "type": "AcceptP2POffer", "offer_id": offer_id })),
        )
        .unwrap();
        assert_eq!(global_amount(&world, 1, "Ore"), 10);
        assert_eq!(global_amount(&world, 2, "Energy"), 130);

        let auction_id = settlement_id(1, 12);
        run_settlement_action(
            &mut world,
            1,
            1,
            CommandSource::TestHarness,
            wire_action(serde_json::json!({
                "type": "CreateAuction",
                "auction_id": auction_id,
                "nonce": 12,
                "lot_resource": "Energy",
                "lot_amount": 20,
                "bid_resource": "Ore",
                "min_bid": 5,
                "ends_at": 3,
            })),
        )
        .unwrap();
        run_settlement_action(
            &mut world,
            2,
            1,
            CommandSource::TestHarness,
            wire_action(serde_json::json!({
                "type": "BidAuction",
                "auction_id": auction_id,
                "bid_amount": 8,
            })),
        )
        .unwrap();
        run_settlement_action(
            &mut world,
            3,
            1,
            CommandSource::TestHarness,
            wire_action(serde_json::json!({
                "type": "BidAuction",
                "auction_id": auction_id,
                "bid_amount": 9,
            })),
        )
        .unwrap();
        assert_eq!(global_amount(&world, 2, "Ore"), 90);
        world.app.world_mut().resource_mut::<CurrentTick>().0 = 3;
        assert!(
            run_settlement_action(
                &mut world,
                2,
                3,
                CommandSource::TestHarness,
                wire_action(serde_json::json!({
                    "type": "BidAuction",
                    "auction_id": auction_id,
                    "bid_amount": 9,
                }))
            )
            .is_err()
        );
        world.app.world_mut().resource_mut::<CurrentTick>().0 = 3;
        run_settlement_action(
            &mut world,
            1,
            3,
            CommandSource::TestHarness,
            wire_action(serde_json::json!({ "type": "SettleAuction", "auction_id": auction_id })),
        )
        .unwrap();
        assert_eq!(global_amount(&world, 2, "Energy"), 130);
        assert_eq!(global_amount(&world, 3, "Energy"), 20);

        let escrow_id = settlement_id(1, 13);
        run_settlement_action(
            &mut world,
            1,
            3,
            CommandSource::TestHarness,
            wire_action(serde_json::json!({
                "type": "CreateEscrow",
                "escrow_id": escrow_id,
                "nonce": 13,
                "payee": 2,
                "arbiter": 3,
                "resource": "Energy",
                "amount": 10,
            })),
        )
        .unwrap();
        assert!(
            run_settlement_action(
                &mut world,
                1,
                3,
                CommandSource::TestHarness,
                wire_action(serde_json::json!({ "type": "RefundEscrow", "escrow_id": escrow_id }))
            )
            .is_err()
        );
        run_settlement_action(
            &mut world,
            3,
            3,
            CommandSource::TestHarness,
            wire_action(serde_json::json!({ "type": "ReleaseEscrow", "escrow_id": escrow_id })),
        )
        .unwrap();
        assert_eq!(global_amount(&world, 2, "Energy"), 140);

        let loan_id = settlement_id(1, 14);
        run_settlement_action(
            &mut world,
            1,
            3,
            CommandSource::TestHarness,
            wire_action(serde_json::json!({
                "type": "CreateLoanOffer",
                "loan_id": loan_id,
                "nonce": 14,
                "borrower": 2,
                "resource": "Energy",
                "principal": 10,
                "repay_amount": 12,
                "due_at": 6,
            })),
        )
        .unwrap();
        run_settlement_action(
            &mut world,
            2,
            3,
            CommandSource::TestHarness,
            wire_action(serde_json::json!({ "type": "AcceptLoan", "loan_id": loan_id })),
        )
        .unwrap();
        run_settlement_action(
            &mut world,
            2,
            4,
            CommandSource::TestHarness,
            wire_action(serde_json::json!({ "type": "RepayLoan", "loan_id": loan_id })),
        )
        .unwrap();
        assert_eq!(
            world
                .app
                .world()
                .resource::<SettlementState>()
                .loans
                .get(&loan_id)
                .unwrap()
                .status,
            SettlementStatus::Repaid
        );

        let total_energy = global_amount(&world, 1, "Energy")
            + global_amount(&world, 2, "Energy")
            + global_amount(&world, 3, "Energy")
            + world
                .app
                .world()
                .resource::<SettlementState>()
                .reserves
                .values()
                .map(|cost| cost.get("Energy").copied().unwrap_or_default())
                .sum::<u32>();
        assert_eq!(total_energy, 300);

        let ledger = world
            .app
            .world()
            .resource::<ResourceLedger>()
            .trace_snapshot();
        assert!(ledger.conservation_imbalance.is_empty());
        assert!(ledger.operations.iter().any(|op| {
            matches!(
                (&op.source_account, &op.target_account),
                (
                    Some(LedgerAccount::Player { player_id: 1 }),
                    Some(LedgerAccount::Reserve { kind: SettlementKind::P2POffer, id: reserve_id, label })
                ) if *reserve_id == offer_id && label == "give" && op.resource == "Energy" && op.amount_requested == 30
            )
        }));
        assert!(ledger.operations.iter().any(|op| {
            matches!(
                (&op.source_account, &op.target_account),
                (
                    Some(LedgerAccount::Player { player_id: 2 }),
                    Some(LedgerAccount::Player { player_id: 1 })
            ) if op.operation == ResourceOperation::plugin_settlement(PluginSettlementKind::P2POffer) && op.resource == "Ore" && op.amount_requested == 10
            )
        }));
        assert!(ledger.operations.iter().any(|op| {
            matches!(
                (&op.source_account, &op.target_account),
                (
                    Some(LedgerAccount::Reserve { kind: SettlementKind::Auction, id: reserve_id, label }),
                    Some(LedgerAccount::Player { player_id: 2 })
                ) if *reserve_id == auction_id && label == "bid" && op.resource == "Ore" && op.amount_requested == 8
            )
        }));
        assert!(ledger.operations.iter().any(|op| {
            matches!(
                (&op.source_account, &op.target_account),
                (
                    Some(LedgerAccount::Reserve { kind: SettlementKind::Auction, id: reserve_id, label }),
                    Some(LedgerAccount::Player { player_id: 3 })
                ) if *reserve_id == auction_id && label == "lot" && op.resource == "Energy" && op.amount_requested == 20
            )
        }));
        assert!(ledger.operations.iter().any(|op| {
            matches!(
                (&op.source_account, &op.target_account),
                (
                    Some(LedgerAccount::Reserve { kind: SettlementKind::Escrow, id: reserve_id, label }),
                    Some(LedgerAccount::Player { player_id: 2 })
                ) if *reserve_id == escrow_id && label == "escrow" && op.resource == "Energy" && op.amount_requested == 10
            )
        }));
        assert!(ledger.operations.iter().any(|op| {
            matches!(
                (&op.source_account, &op.target_account),
                (
                    Some(LedgerAccount::Reserve { kind: SettlementKind::Lending, id: reserve_id, label }),
                    Some(LedgerAccount::Player { player_id: 2 })
                ) if *reserve_id == loan_id && label == "principal" && op.resource == "Energy" && op.amount_requested == 10
            )
        }));
        assert!(ledger.operations.iter().any(|op| {
            matches!(
                (&op.source_account, &op.target_account),
                (
                    Some(LedgerAccount::Player { player_id: 2 }),
                    Some(LedgerAccount::Player { player_id: 1 })
            ) if op.operation == ResourceOperation::plugin_settlement(PluginSettlementKind::Lending) && op.resource == "Energy" && op.amount_requested == 12
            )
        }));
    }

    #[test]
    fn settlement_state_is_snapshot_restored_and_schema_exports_commands() {
        let mut world = create_world();
        set_global(&mut world, 1, "Energy", 100);
        let before = crate::world::state_checksum(world.app.world_mut());
        let id = settlement_id(1, 21);
        run_settlement_action(
            &mut world,
            1,
            1,
            CommandSource::TestHarness,
            wire_action(serde_json::json!({
                "type": "CreateEscrow",
                "escrow_id": id,
                "nonce": 21,
                "payee": 2,
                "arbiter": 3,
                "resource": "Energy",
                "amount": 10,
            })),
        )
        .unwrap();
        let after = crate::world::state_checksum(world.app.world_mut());
        assert_ne!(before, after);
        let snapshot = crate::tick::WorldSnapshot::capture(world.app.world_mut());
        world
            .app
            .world_mut()
            .resource_mut::<SettlementState>()
            .escrows
            .clear();
        snapshot.restore(world.app.world_mut());
        assert!(
            world
                .app
                .world()
                .resource::<SettlementState>()
                .escrows
                .contains_key(&id)
        );

        assert!(serde_json::from_value::<SettlementState>(serde_json::json!({})).is_ok());

        let schema = serde_json::to_value(schemars::schema_for!(Vec<CommandIntent>)).unwrap();
        let tags = schema["$defs"]["CommandAction"]["oneOf"]
            .as_array()
            .unwrap()
            .iter()
            .filter_map(|branch| branch["properties"]["type"]["const"].as_str())
            .collect::<Vec<_>>();
        assert!(!tags.contains(&"CreateContractSettlement"));
        assert!(!tags.contains(&"DefaultLoan"));
        assert!(!tags.contains(&"Action"));
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
            panic!("registered action must deserialize to generic Action");
        };
        assert_eq!(action_type, "Debilitate");
        assert_eq!(object_id, 7);
        assert_eq!(target_id, Some(9));
        assert_eq!(
            payload
                .get("damage_type")
                .and_then(serde_json::Value::as_str),
            Some("Kinetic")
        );
        assert_eq!(
            payload.get("cooldown").and_then(serde_json::Value::as_u64),
            Some(5)
        );
    }

    #[test]
    fn every_special_action_decodes_legacy_and_serializes_canonical_wire() {
        for &action_type in SPECIAL_COMMAND_ACTIONS {
            let canonical = serde_json::json!({
                "type": action_type,
                "object_id": 7,
                "target_id": 9,
                "resource": "Energy",
                "amount": 3,
                "range": 2,
                "structure": "Tower",
                "damage_type": "EMP",
                "cooldown": 5
            });
            let direct = serde_json::from_value::<CommandAction>(canonical.clone()).unwrap();
            let expected = special_action_from_name::<serde::de::value::Error>(
                action_type,
                SpecialActionFields {
                    object_id: 7,
                    target_id: 9,
                    resource: Some("Energy".to_string()),
                    amount: Some(3),
                    range: Some(2),
                    structure: Some(StructureType::TOWER),
                    damage_type: Some("EMP".to_string()),
                    cooldown: Some(5),
                },
            )
            .unwrap();
            assert_eq!(direct, expected);

            let legacy = serde_json::from_value::<CommandAction>(serde_json::json!({
                "type": "Action",
                "action_type": action_type,
                "object_id": 7,
                "target_id": 9,
                "payload": {
                    "resource": "Energy",
                    "amount": 3,
                    "range": 2,
                    "structure": "Tower",
                    "damage_type": "EMP",
                    "cooldown": 5
                }
            }))
            .unwrap();
            assert_eq!(legacy, expected);

            let serialized = serde_json::to_value(&legacy).unwrap();
            assert_eq!(serialized, canonical);
            assert!(serialized.get("action_type").is_none());
            assert!(serialized.get("payload").is_none());
        }
    }

    #[test]
    fn generated_command_schema_tags_match_special_action_registry() {
        let schema = serde_json::to_value(schemars::schema_for!(Vec<CommandIntent>)).unwrap();
        let tags = schema["$defs"]["CommandAction"]["oneOf"]
            .as_array()
            .unwrap()
            .iter()
            .filter_map(|branch| branch["properties"]["type"]["const"].as_str())
            .collect::<Vec<_>>();
        let special_tags = tags
            .iter()
            .copied()
            .filter(|tag| SPECIAL_COMMAND_ACTIONS.contains(tag))
            .collect::<Vec<_>>();

        assert!(special_tags.is_empty());
        assert!(!tags.contains(&"Action"));
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
    fn legacy_action_wrapper_normalizes_action_type_and_rejects_reserved_payload_keys() {
        let legacy = serde_json::from_value::<CommandAction>(serde_json::json!({
            "type": "Action",
            "action_type": "Attack",
            "object_id": 7,
            "target_id": 9
        }))
        .unwrap();
        assert_eq!(
            legacy,
            CommandAction::Action {
                action_type: "Attack".to_string(),
                object_id: 7,
                target_id: Some(9),
                payload: serde_json::json!({}),
            }
        );

        let unknown = serde_json::from_value::<CommandAction>(serde_json::json!({
            "type": "Action",
            "action_type": "Move",
            "object_id": 7,
            "target_id": 9
        }))
        .unwrap_err();
        assert!(
            unknown
                .to_string()
                .contains("legacy Action wrapper uses reserved non-special action_type Move")
        );

        let reserved_payload = serde_json::from_value::<CommandAction>(serde_json::json!({
            "type": "Action",
            "action_type": "Attack",
            "object_id": 7,
            "target_id": 9,
            "payload": {"type": "Move"}
        }))
        .unwrap_err();
        assert!(reserved_payload.to_string().contains("reserved field type"));

        assert!(
            serde_json::from_value::<CommandAction>(serde_json::json!({
                "type": "Attack",
                "object_id": 7,
                "target_id": 9,
                "payload": {"cooldown": 1}
            }))
            .is_err()
        );
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
            .submit_raw_command(RawCommand {
                player_id: 1,
                tick: 1,
                source: CommandSource::TestHarness,
                auth: CommandAuth::server_injected(CommandSource::TestHarness, 1, 1, 1),
                sequence: 1,
                action: CommandAction::Action {
                    action_type: "Disrupt".to_string(),
                    object_id: attacker_id,
                    target_id: Some(target_id),
                    payload: serde_json::json!({}),
                },
            })
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
            .submit_raw_command(RawCommand {
                player_id: 1,
                tick: 1,
                source: CommandSource::TestHarness,
                auth: CommandAuth::server_injected(CommandSource::TestHarness, 1, 1, 1),
                sequence: 1,
                action: CommandAction::Action {
                    action_type: "Fortify".to_string(),
                    object_id: drone_id,
                    target_id: Some(drone_id),
                    payload: serde_json::json!({}),
                },
            })
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

    #[test]
    fn allied_transfer_records_gross_delivered_and_fee_once() {
        let mut world = create_world();
        set_global(&mut world, 1, "Energy", 1_000);
        seed_first_spawn(&mut world, 2);

        submit_transfer_action(
            &mut world,
            1,
            CommandAction::AlliedTransfer {
                target_player: 2,
                resource: "Energy".into(),
                amount: 100,
            },
        );

        let ledger = world.app.world().resource::<ResourceLedger>();
        let entry = ledger.ops.last().unwrap();
        assert_eq!(entry.operation, ResourceOperation::AlliedTransfer);
        assert_eq!(entry.amount_requested, 100);
        assert_eq!(entry.amount_delivered, 98);
        assert_eq!(entry.fee_paid, 2);
        assert_eq!(entry.basis_points_used, ALLIED_TRANSFER_FEE_BP);
        assert_eq!(
            *ledger.balance_delta.get(&1).unwrap().get("Energy").unwrap(),
            -100
        );
        assert_eq!(
            *ledger.balance_delta.get(&2).unwrap().get("Energy").unwrap(),
            98
        );
        assert_eq!(
            *ledger
                .account_delta
                .get("player:1")
                .unwrap()
                .get("Energy")
                .unwrap(),
            -100
        );
        assert_eq!(
            *ledger
                .account_delta
                .get("player:2")
                .unwrap()
                .get("Energy")
                .unwrap(),
            98
        );
        assert_eq!(
            *ledger
                .account_delta
                .get("sink:AlliedTransfer:fee")
                .unwrap()
                .get("Energy")
                .unwrap(),
            2
        );
    }

    #[test]
    fn transfer_to_global_records_gross_delivered_and_fee_once() {
        let mut world = create_world();
        give_local_energy(world.app.world_mut(), 1, 1_000);

        submit_transfer_action(
            &mut world,
            1,
            CommandAction::TransferToGlobal {
                resource: "Energy".into(),
                amount: 100,
            },
        );

        {
            let ledger = world.app.world().resource::<ResourceLedger>();
            let entry = ledger.ops.last().unwrap();
            assert_eq!(entry.operation, ResourceOperation::GlobalDeposit);
            assert_eq!(entry.amount_requested, 100);
            assert_eq!(entry.amount_delivered, 99);
            assert_eq!(entry.fee_paid, 1);
            assert_eq!(entry.basis_points_used, 100);
            assert_eq!(
                *ledger.balance_delta.get(&1).unwrap().get("Energy").unwrap(),
                -1
            );
            assert_eq!(
                *ledger
                    .account_delta
                    .get("player:1")
                    .unwrap()
                    .get("Energy")
                    .unwrap(),
                -1
            );
            assert_eq!(
                *ledger
                    .account_delta
                    .get("sink:GlobalDeposit:fee")
                    .unwrap()
                    .get("Energy")
                    .unwrap(),
                1
            );
        }
        assert_eq!(local_amount(&world, 1, "Energy"), 900);
        {
            let pending = world.app.world().resource::<PendingGlobalTransfers>();
            let transfer = pending.0.last().unwrap();
            assert_eq!(transfer.amount, 100);
            assert_eq!(transfer.deliver_amount, 99);
        }
    }

    #[test]
    fn transfer_from_global_records_gross_delivered_and_fee_once() {
        let mut world = create_world();
        set_global(&mut world, 1, "Energy", 1_000);

        submit_transfer_action(
            &mut world,
            1,
            CommandAction::TransferFromGlobal {
                resource: "Energy".into(),
                amount: 1_000,
            },
        );

        {
            let ledger = world.app.world().resource::<ResourceLedger>();
            let entry = ledger.ops.last().unwrap();
            assert_eq!(entry.operation, ResourceOperation::GlobalWithdraw);
            assert_eq!(entry.amount_requested, 1_000);
            assert_eq!(entry.amount_delivered, 990);
            assert_eq!(entry.fee_paid, 10);
            assert_eq!(entry.basis_points_used, 100);
            assert_eq!(
                *ledger.balance_delta.get(&1).unwrap().get("Energy").unwrap(),
                -10
            );
            assert_eq!(
                *ledger
                    .account_delta
                    .get("player:1")
                    .unwrap()
                    .get("Energy")
                    .unwrap(),
                -10
            );
            assert_eq!(
                *ledger
                    .account_delta
                    .get("sink:GlobalWithdraw:fee")
                    .unwrap()
                    .get("Energy")
                    .unwrap(),
                10
            );
        }
        {
            let pending = world.app.world().resource::<PendingGlobalTransfers>();
            let transfer = pending.0.last().unwrap();
            assert_eq!(transfer.amount, 1_000);
            assert_eq!(transfer.deliver_amount, 990);
        }
    }
}
