export type UInt32 = number;
export type UInt64 = number | bigint;
export type PlayerId = UInt32;
export type RoomId = UInt32;
export type ObjectId = UInt64;
export type Tick = UInt64;
export type ResourceName = string;
export type ResourceAmount = UInt32;
export type ResourceCost = Record<ResourceName, ResourceAmount>;
export interface Position {
    x: number;
    y: number;
    room: RoomId;
}
export type Direction = "North" | "South" | "East" | "West";
export type BodyPart = "Move" | "Work" | "Carry" | "Attack" | "RangedAttack" | "Heal" | "Claim" | "Tough";
export type DamageType = "Kinetic" | "Thermal" | "EMP" | "Sonic" | "Corrosive" | "Psionic";
export type RejectionReasonCode = "InvalidJson" | "SchemaViolation" | "ObjectNotFound" | "NotOwner" | "InsufficientResource" | "OutOfRange" | "NotStructure" | "NotController" | "NotVisibleOrNotFound" | "TargetNotVisible" | "SpawnOnCooldown" | "RoomDroneCapReached" | "AuthContextInvalid" | "CooldownActive" | "InvalidDirection" | "PositionOccupied" | "CapacityExceeded" | "ConstructionLimitReached" | "SafeModeActive" | "TargetOverloadCooldown" | "TargetFortifyCooldown" | "NotEnoughBodyParts" | "InvalidBodyPart" | "InvalidStructureType" | "InvalidResourceType" | "SourceNotAllowed" | "UnknownAction" | "GlobalStorageDisabled" | "TransferInProgress" | "RateLimited" | "InvalidCertificate" | "NotAuthorized" | "FuelExhausted" | "TimeoutExceeded" | "SnapshotOverBudget" | "CommandBufferFull" | "ServerOverloaded" | "InternalError";
export type StructureType = "Spawn" | "Extension" | "Tower" | "Storage" | "Link" | "Extractor" | "Lab" | "Terminal" | "Nuker" | "Observer" | "PowerSpawn" | "Factory" | "Depot";
export interface MoveAction {
    type: "Move";
    object_id: ObjectId;
    direction: Direction;
}
export interface HarvestAction {
    type: "Harvest";
    object_id: ObjectId;
    target_id: ObjectId;
    resource?: ResourceName;
}
export interface TransferAction {
    type: "Transfer";
    object_id: ObjectId;
    target_id: ObjectId;
    resource: ResourceName;
    amount: ResourceAmount;
}
export interface WithdrawAction {
    type: "Withdraw";
    object_id: ObjectId;
    target_id: ObjectId;
    resource: ResourceName;
    amount: ResourceAmount;
}
export interface ClaimControllerAction {
    type: "ClaimController";
    object_id: ObjectId;
    target_id: ObjectId;
}
export interface SpawnAction {
    type: "Spawn";
    object_id: ObjectId;
    spawn_id: ObjectId;
    body_parts: BodyPart[];
}
export interface RecycleAction {
    type: "Recycle";
    object_id: ObjectId;
}
export interface BuildAction {
    type: "Build";
    object_id: ObjectId;
    x: number;
    y: number;
    structure: StructureType;
}
export interface RepairAction {
    type: "Repair";
    object_id: ObjectId;
    target_id: ObjectId;
}
export interface UpgradeControllerAction {
    type: "UpgradeController";
    object_id: ObjectId;
    target_id: ObjectId;
}
export interface TransferToGlobalAction {
    type: "TransferToGlobal";
    resource: ResourceName;
    amount: ResourceAmount;
}
export interface TransferFromGlobalAction {
    type: "TransferFromGlobal";
    resource: ResourceName;
    amount: ResourceAmount;
}
export interface AlliedTransferAction {
    type: "AlliedTransfer";
    target_player: PlayerId;
    resource: ResourceName;
    amount: ResourceAmount;
}
export interface AttackAction {
    type: "Attack";
    object_id: ObjectId;
    target_id: ObjectId;
    resource?: ResourceName;
    amount?: ResourceAmount;
    range?: number;
    structure?: StructureType;
    damage_type?: DamageType;
    cooldown?: number;
}
export interface RangedAttackAction {
    type: "RangedAttack";
    object_id: ObjectId;
    target_id: ObjectId;
    resource?: ResourceName;
    amount?: ResourceAmount;
    range?: number;
    structure?: StructureType;
    damage_type?: DamageType;
    cooldown?: number;
}
export interface HealAction {
    type: "Heal";
    object_id: ObjectId;
    target_id: ObjectId;
    resource?: ResourceName;
    amount?: ResourceAmount;
    range?: number;
    structure?: StructureType;
    damage_type?: DamageType;
    cooldown?: number;
}
export interface HackAction {
    type: "Hack";
    object_id: ObjectId;
    target_id: ObjectId;
    resource?: ResourceName;
    amount?: ResourceAmount;
    range?: number;
    structure?: StructureType;
    damage_type?: DamageType;
    cooldown?: number;
}
export interface DrainAction {
    type: "Drain";
    object_id: ObjectId;
    target_id: ObjectId;
    resource?: ResourceName;
    amount?: ResourceAmount;
    range?: number;
    structure?: StructureType;
    damage_type?: DamageType;
    cooldown?: number;
}
export interface OverloadAction {
    type: "Overload";
    object_id: ObjectId;
    target_id: ObjectId;
    resource?: ResourceName;
    amount?: ResourceAmount;
    range?: number;
    structure?: StructureType;
    damage_type?: DamageType;
    cooldown?: number;
}
export interface DebilitateAction {
    type: "Debilitate";
    object_id: ObjectId;
    target_id: ObjectId;
    resource?: ResourceName;
    amount?: ResourceAmount;
    range?: number;
    structure?: StructureType;
    damage_type?: DamageType;
    cooldown?: number;
}
export interface DisruptAction {
    type: "Disrupt";
    object_id: ObjectId;
    target_id: ObjectId;
    resource?: ResourceName;
    amount?: ResourceAmount;
    range?: number;
    structure?: StructureType;
    damage_type?: DamageType;
    cooldown?: number;
}
export interface FortifyAction {
    type: "Fortify";
    object_id: ObjectId;
    target_id: ObjectId;
    resource?: ResourceName;
    amount?: ResourceAmount;
    range?: number;
    structure?: StructureType;
    damage_type?: DamageType;
    cooldown?: number;
}
export interface LeechAction {
    type: "Leech";
    object_id: ObjectId;
    target_id: ObjectId;
    resource?: ResourceName;
    amount?: ResourceAmount;
    range?: number;
    structure?: StructureType;
    damage_type?: DamageType;
    cooldown?: number;
}
export interface FabricateAction {
    type: "Fabricate";
    object_id: ObjectId;
    target_id: ObjectId;
    resource?: ResourceName;
    amount?: ResourceAmount;
    range?: number;
    structure?: StructureType;
    damage_type?: DamageType;
    cooldown?: number;
}
export type Action = MoveAction | HarvestAction | TransferAction | WithdrawAction | ClaimControllerAction | SpawnAction | RecycleAction | BuildAction | RepairAction | UpgradeControllerAction | TransferToGlobalAction | TransferFromGlobalAction | AlliedTransferAction | AttackAction | RangedAttackAction | HealAction | HackAction | DrainAction | OverloadAction | DebilitateAction | DisruptAction | FortifyAction | LeechAction | FabricateAction;
export interface CommandIntent<A extends Action = Action> {
    sequence: UInt32;
    idempotency_key: string;
    client_trace_id?: string;
    action: A;
}
export interface TickInput {
    tick: UInt64;
    player_id: PlayerId;
    world_id: UInt64;
    visible_snapshot: Uint8Array;
    world_config_view: {
        config_hash: Uint8Array;
        payload: Uint8Array;
    };
    fuel_budget_hints: {
        fuel_remaining: UInt64;
        host_calls_remaining: UInt32;
        output_bytes_remaining: UInt32;
    };
    message_inbox_cursor: {
        next_message_id: UInt64;
    };
}
export interface TickResult<A extends Action = Action> {
    commands: CommandIntent<A>[];
    messages: PlayerMessage[];
}
export interface PlayerMessage {
    channel: "Player" | "Debug";
    text: string;
}
export interface SpecialActionPayload {
    resource?: ResourceName;
    amount?: ResourceAmount;
    range?: number;
    structure?: StructureType;
    damage_type?: DamageType;
    cooldown?: number;
}
export declare function addCost(target: ResourceCost, source: ResourceCost): ResourceCost;
export declare function command<A extends Action>(sequence: number, idempotency_key: string, action: A, client_trace_id?: string): CommandIntent<A>;
export declare const actions: Readonly<{
    move: (object_id: ObjectId, direction: Direction) => {
        readonly type: "Move";
        readonly object_id: UInt64;
        readonly direction: Direction;
    };
    harvest: (object_id: ObjectId, target_id: ObjectId, resource?: ResourceName) => {
        readonly type: "Harvest";
        readonly object_id: UInt64;
        readonly target_id: UInt64;
        readonly resource: string | undefined;
    };
    transfer: (object_id: ObjectId, target_id: ObjectId, resource: ResourceName, amount: ResourceAmount) => {
        readonly type: "Transfer";
        readonly object_id: UInt64;
        readonly target_id: UInt64;
        readonly resource: string;
        readonly amount: number;
    };
    withdraw: (object_id: ObjectId, target_id: ObjectId, resource: ResourceName, amount: ResourceAmount) => {
        readonly type: "Withdraw";
        readonly object_id: UInt64;
        readonly target_id: UInt64;
        readonly resource: string;
        readonly amount: number;
    };
    claimController: (object_id: ObjectId, target_id: ObjectId) => {
        readonly type: "ClaimController";
        readonly object_id: UInt64;
        readonly target_id: UInt64;
    };
    spawn: (object_id: ObjectId, spawn_id: ObjectId, body_parts: BodyPart[]) => {
        readonly type: "Spawn";
        readonly object_id: UInt64;
        readonly spawn_id: UInt64;
        readonly body_parts: BodyPart[];
    };
    recycle: (object_id: ObjectId) => {
        readonly type: "Recycle";
        readonly object_id: UInt64;
    };
    build: (object_id: ObjectId, x: number, y: number, structure: StructureType) => {
        readonly type: "Build";
        readonly object_id: UInt64;
        readonly x: number;
        readonly y: number;
        readonly structure: StructureType;
    };
    repair: (object_id: ObjectId, target_id: ObjectId) => {
        readonly type: "Repair";
        readonly object_id: UInt64;
        readonly target_id: UInt64;
    };
    upgradeController: (object_id: ObjectId, target_id: ObjectId) => {
        readonly type: "UpgradeController";
        readonly object_id: UInt64;
        readonly target_id: UInt64;
    };
    transferToGlobal: (resource: ResourceName, amount: ResourceAmount) => {
        readonly type: "TransferToGlobal";
        readonly resource: string;
        readonly amount: number;
    };
    transferFromGlobal: (resource: ResourceName, amount: ResourceAmount) => {
        readonly type: "TransferFromGlobal";
        readonly resource: string;
        readonly amount: number;
    };
    alliedTransfer: (target_player: PlayerId, resource: ResourceName, amount: ResourceAmount) => {
        readonly type: "AlliedTransfer";
        readonly target_player: number;
        readonly resource: string;
        readonly amount: number;
    };
    attack: (object_id: ObjectId, target_id: ObjectId, payload?: SpecialActionPayload) => {
        readonly resource?: ResourceName;
        readonly amount?: ResourceAmount;
        readonly range?: number;
        readonly structure?: StructureType;
        readonly damage_type?: DamageType;
        readonly cooldown?: number;
        readonly type: "Attack";
        readonly object_id: UInt64;
        readonly target_id: UInt64;
    };
    rangedAttack: (object_id: ObjectId, target_id: ObjectId, payload?: SpecialActionPayload) => {
        readonly resource?: ResourceName;
        readonly amount?: ResourceAmount;
        readonly range?: number;
        readonly structure?: StructureType;
        readonly damage_type?: DamageType;
        readonly cooldown?: number;
        readonly type: "RangedAttack";
        readonly object_id: UInt64;
        readonly target_id: UInt64;
    };
    heal: (object_id: ObjectId, target_id: ObjectId, payload?: SpecialActionPayload) => {
        readonly resource?: ResourceName;
        readonly amount?: ResourceAmount;
        readonly range?: number;
        readonly structure?: StructureType;
        readonly damage_type?: DamageType;
        readonly cooldown?: number;
        readonly type: "Heal";
        readonly object_id: UInt64;
        readonly target_id: UInt64;
    };
    hack: (object_id: ObjectId, target_id: ObjectId, payload?: SpecialActionPayload) => {
        readonly resource?: ResourceName;
        readonly amount?: ResourceAmount;
        readonly range?: number;
        readonly structure?: StructureType;
        readonly damage_type?: DamageType;
        readonly cooldown?: number;
        readonly type: "Hack";
        readonly object_id: UInt64;
        readonly target_id: UInt64;
    };
    drain: (object_id: ObjectId, target_id: ObjectId, payload?: SpecialActionPayload) => {
        readonly resource?: ResourceName;
        readonly amount?: ResourceAmount;
        readonly range?: number;
        readonly structure?: StructureType;
        readonly damage_type?: DamageType;
        readonly cooldown?: number;
        readonly type: "Drain";
        readonly object_id: UInt64;
        readonly target_id: UInt64;
    };
    overload: (object_id: ObjectId, target_id: ObjectId, payload?: SpecialActionPayload) => {
        readonly resource?: ResourceName;
        readonly amount?: ResourceAmount;
        readonly range?: number;
        readonly structure?: StructureType;
        readonly damage_type?: DamageType;
        readonly cooldown?: number;
        readonly type: "Overload";
        readonly object_id: UInt64;
        readonly target_id: UInt64;
    };
    debilitate: (object_id: ObjectId, target_id: ObjectId, payload?: SpecialActionPayload) => {
        readonly resource?: ResourceName;
        readonly amount?: ResourceAmount;
        readonly range?: number;
        readonly structure?: StructureType;
        readonly damage_type?: DamageType;
        readonly cooldown?: number;
        readonly type: "Debilitate";
        readonly object_id: UInt64;
        readonly target_id: UInt64;
    };
    disrupt: (object_id: ObjectId, target_id: ObjectId, payload?: SpecialActionPayload) => {
        readonly resource?: ResourceName;
        readonly amount?: ResourceAmount;
        readonly range?: number;
        readonly structure?: StructureType;
        readonly damage_type?: DamageType;
        readonly cooldown?: number;
        readonly type: "Disrupt";
        readonly object_id: UInt64;
        readonly target_id: UInt64;
    };
    fortify: (object_id: ObjectId, target_id: ObjectId, payload?: SpecialActionPayload) => {
        readonly resource?: ResourceName;
        readonly amount?: ResourceAmount;
        readonly range?: number;
        readonly structure?: StructureType;
        readonly damage_type?: DamageType;
        readonly cooldown?: number;
        readonly type: "Fortify";
        readonly object_id: UInt64;
        readonly target_id: UInt64;
    };
    leech: (object_id: ObjectId, target_id: ObjectId, payload?: SpecialActionPayload) => {
        readonly resource?: ResourceName;
        readonly amount?: ResourceAmount;
        readonly range?: number;
        readonly structure?: StructureType;
        readonly damage_type?: DamageType;
        readonly cooldown?: number;
        readonly type: "Leech";
        readonly object_id: UInt64;
        readonly target_id: UInt64;
    };
    fabricate: (object_id: ObjectId, target_id: ObjectId, payload?: SpecialActionPayload) => {
        readonly resource?: ResourceName;
        readonly amount?: ResourceAmount;
        readonly range?: number;
        readonly structure?: StructureType;
        readonly damage_type?: DamageType;
        readonly cooldown?: number;
        readonly type: "Fabricate";
        readonly object_id: UInt64;
        readonly target_id: UInt64;
    };
}>;
export declare const BODY_PART_COST: {
    readonly Attack: {
        readonly Energy: 80;
    };
    readonly Carry: {
        readonly Energy: 50;
    };
    readonly Claim: {
        readonly Energy: 600;
    };
    readonly Heal: {
        readonly Energy: 250;
    };
    readonly Move: {
        readonly Energy: 50;
    };
    readonly RangedAttack: {
        readonly Energy: 100;
    };
    readonly Tough: {
        readonly Energy: 10;
    };
    readonly Work: {
        readonly Energy: 100;
    };
};
export declare function bodyCost(body: BodyPart[]): ResourceCost;
export interface RealtimePosition {
    x: number;
    y: number;
    room_id: RoomId;
}
export interface RealtimeVisibleEntity {
    type: "Drone" | "Structure" | "Source" | "Resource" | "Controller";
    id: ObjectId;
    position: RealtimePosition;
    owner?: PlayerId | null;
    structure_type?: string;
    body?: BodyPart[];
    carry?: ResourceCost;
    hits?: UInt32;
    energy?: UInt32 | null;
    produces?: ResourceCost;
    amounts?: ResourceCost;
    level?: UInt32;
}
export interface RealtimePayloadV1 {
    tick: Tick;
    last_tick: Tick;
    player_id: PlayerId;
    full_snapshot: boolean;
    changed_entities: RealtimeVisibleEntity[];
    removed_entities: ObjectId[];
    state_checksum: UInt64;
}
export interface RealtimeEnvelopeV1 {
    schema: "swarm.realtime.v1";
    payload: RealtimePayloadV1;
}
export declare const SDK_VERSION = "0.1.0";
export declare const IDL_VERSION = "1.0.0";
export declare const ABI_VERSION = 2;
export declare const AI_GAME_DATA_START = "___AI_GAME_DATA_START___";
export declare const AI_GAME_DATA_END = "___AI_GAME_DATA_END___";
export declare const MAX_BODY_PARTS = 50;
export declare const MAX_COMMANDS_PER_PLAYER = 100;
export declare const MAX_DRONES_PER_PLAYER = 500;
export declare const MAX_FUEL = 10000000;
export declare const MAX_JSON_DEPTH = 10;
export declare const MAX_NEXT_TICK_FUEL_BUDGET = 11000000;
export declare const MAX_RANGED_ATTACK_RANGE = 3;
export declare const MAX_REFUND_PER_TICK = 1000000;
export declare const MAX_TICK_OUTPUT_BYTES = 262144;
//# sourceMappingURL=commands.d.ts.map