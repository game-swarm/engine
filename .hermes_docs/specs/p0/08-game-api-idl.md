# P0-8: Game API IDL Spec — 游戏 API 接口定义语言

> **状态**: Phase 0 Architecture Freeze | **目标**: host functions / Command / Validator / SDK / MCP schema 单一真相来源

## 1. 原则

**一个 IDL 生成所有绑定——不一致即编译错误。**

```
game_api.idl  (单一真相)
    │
    ├──→ Rust:   host function stubs + Command enum + Validator trait
    ├──→ TS:     SDK types + autocomplete
    ├──→ MCP:    tool schemas + docs resources
    ├──→ Docs:   API reference (human + AI)
    └──→ Test:   property-based test generators
```

**IDL 定义的指令类型是 CommandIntent**——即 WASM 模块 `tick()` 的可信输出格式。CommandIntent 仅包含 `sequence` + `action` 两个字段。`player_id`、`source`、`tick` 等身份/时序字段由服务端 Source Gate 注入后形成 RawCommand（见 P0-2 §2）。IDL 不定义 RawCommand 的 envelope 字段——那些是引擎内部结构。所有校验规则（`validator` 数组）定义在 CommandIntent 的 `action` 字段上。

## 2. IDL 格式

```yaml
# game_api.idl — Swarm Game API Interface Definition

version: "1.0.0"
abi_version: 1                # 每次 host function 签名变更时递增
generated: "2026-06-14"

types:
  PlayerId: u32
  RoomId: u32
  ObjectId: u64
  Tick: u64
  ResourceName: String
  ResourceAmount: u32
  ResourceCost: Map<ResourceName, ResourceAmount>
  Position: { x: i32, y: i32, room: RoomId }

enums:
  Direction: [Top, TopRight, BottomRight, Bottom, BottomLeft, TopLeft]
  BodyPart:  [Move, Work, Carry, Attack, RangedAttack, Heal, Claim, Tough]
  DamageType: [Kinetic, Thermal, EMP, Sonic, Corrosive, Psionic]
  StructureType: [Spawn, Extension, Tower, Storage, Link, Extractor, Lab,
                  Terminal, Nuker, Observer, PowerSpawn, Factory]
  RejectionReason:
    - ObjectNotFound
    - NotOwner
    - NotMovable
    - Fatigued
    - MissingBodyPart { part: BodyPart }
    - TileBlocked
    - InvalidDirection
    - StillSpawning
    - OutOfRoom
    - NoPath
    - PathTooLong
    - InsufficientMoveParts
    - CarryFull
    - NotSource
    - SourceEmpty
    - OutOfRange { distance: u32, max: u32 }
    - InsufficientResource { resource: ResourceName, required: u32, available: u32 }
    - TargetFull
    - TargetEmpty
    - NotYourRoom
    - TileOccupied
    - InvalidTerrain
    - TooManyConstructionSites
    - AlreadyFullHealth
    - FriendlyTarget
    - NotYourSpawn
    - SpawnOnCooldown
    - BodyTooLarge
    - ExceedsRoomCapacity
    - RoomDroneCapReached
    - NotFriendly

commands:
  Move:
    params: { object_id: ObjectId, direction: Direction }
    validator: [exists, owner, drone, fatigue, body_part(Move), passable, !spawning]
    cost: {}   # 无资源消耗

  MoveTo:
    params: { object_id: ObjectId, x: i32, y: i32 }
    validator: [Move checks, !spawning, in_room, path_exists, path_length(100)]
    cost: {}   # pathfinding 计入 fuel

  Harvest:
    params: { object_id: ObjectId, target_id: ObjectId, resource: ResourceName? }
    validator: [exists, owner, drone, body_part(Work,Carry), carry_space, is_source, source_not_empty, in_range(1), fatigue]
    cost: {}

  Transfer:
    params: { object_id: ObjectId, target_id: ObjectId, resource: ResourceName, amount: ResourceAmount }
    validator: [exists, owner, drone, body_part(Carry), has_resource, target_has_space, in_range(1)]
    cost: { transfer_amount: amount }

  Withdraw:
    params: { object_id: ObjectId, target_id: ObjectId, resource: ResourceName, amount: ResourceAmount }
    validator: [exists, owner, drone, body_part(Carry), target_has_resource, has_space, in_range(1)]
    cost: { withdraw_amount: amount }

  Build:
    params: { object_id: ObjectId, x: i32, y: i32, structure: StructureType }
    validator: [exists, owner, drone, body_part(Work,Carry), in_your_room, tile_empty, plain_terrain, under_construction_limit(100), in_range(3)]
    cost: registry.build_cost(structure)

  Repair:
    params: { object_id: ObjectId, target_id: ObjectId }
    validator: [exists, owner, drone, body_part(Work,Carry), is_structure, damaged, in_range(3)]
    cost: registry.repair_cost()

  Attack:
    params: { object_id: ObjectId, target_id: ObjectId }
    validator: [exists, owner, drone, body_part(Attack), enemy_target, in_range(1), fatigue]
    cost: {}

  RangedAttack:
    params: { object_id: ObjectId, target_id: ObjectId }
    validator: [exists, owner, drone, body_part(RangedAttack), enemy_target, in_range(3), fatigue]
    cost: {}

  Heal:
    params: { object_id: ObjectId, target_id: ObjectId }
    validator: [exists, owner, drone, body_part(Heal), friendly_target, damaged, in_range(3)]
    cost: {}

  Spawn:
    params: { spawn_id: ObjectId, body: Vec<BodyPart> }
    validator: [exists, owner, is_spawn, cooldown_zero, body_size(50), has_energy(body_cost), room_drone_cap]
    cost: registry.body_cost(body)

  Recycle:
    params: { object_id: ObjectId, spawn_id: ObjectId }
    validator: [exists, owner, drone, is_spawn, in_range(1)]
    refund: registry.body_cost(body) * 0.5

  # ═════════════════════════════════════
  # 特殊攻击（Phase 6 实现，IDL 定义完整校验规则）
  # ═════════════════════════════════════

  Hack:
    params: { object_id: ObjectId, target_id: ObjectId }
    validator: [exists, owner, drone, body_part(Claim), target_drone, not_hacked, in_range(1), fatigue]
    cost: { Energy: 1000 }
    cooldown: 200         # 全局冷却
    description: "施加控制锁逐步夺取 drone——5 tick 渐进控制后转为 Neutral"

  Drain:
    params: { object_id: ObjectId, target_id: ObjectId, resource: ResourceName? }
    validator: [exists, owner, drone, body_part(Work,Carry), target_structure, enemy_target, target_has_resource, carry_space, in_range(1), fatigue]
    cost: { Energy: 200 }
    cooldown: 50          # 每 drone 冷却
    description: "从目标建筑/存储窃取资源，每 tick 转移 carry_capacity 单位"

  Overload:
    params: { object_id: ObjectId, target_id: PlayerId }
    validator: [exists, owner, drone, body_part(RangedAttack), target_player, enemy_target, target_fuel_above(0.2), fatigue]
    cost: { Energy: 300 }
    cooldown: 200         # 每 drone 冷却
    description: "消耗目标 fuel budget 500k，下限 MAX_FUEL×0.2"

  Debilitate:
    params: { object_id: ObjectId, target_id: ObjectId, damage_type: DamageType }
    validator: [exists, owner, drone, body_part(Work), enemy_target, valid_damage_type, not_debilitated(damage_type), in_range(3), fatigue]
    cost: { Energy: 200 }
    cooldown: 150         # 每 drone 冷却
    description: "施加易伤状态——指定伤害类型抗性×2，持续 50 tick"

  Disrupt:
    params: { object_id: ObjectId, target_id: ObjectId }
    validator: [exists, owner, drone, body_part(Attack), target_drone, enemy_target, in_range(1), fatigue]
    cost: { Energy: 100 }
    cooldown: 50          # 每 drone 冷却
    description: "打断目标持续动作（Drain/Hack 控制锁等），不造成伤害"

  Fortify:
    params: { object_id: ObjectId, target_id: ObjectId? }
    validator: [exists, owner, drone, body_part(Tough), target_self_or_ally, in_range(1), fatigue]
    cost: { Energy: 400 }
    cooldown: 300         # 每 drone 冷却
    description: "护盾（所有抗性×0.5）+ 清除目标所有负面状态，持续 100 tick"

# ═════════════════════════════════════
# Body Part 默认成本表（权威来源）
# ═════════════════════════════════════

body_cost:
  Move:         { Energy: 50 }
  Work:         { Energy: 100 }
  Carry:        { Energy: 50 }
  Attack:       { Energy: 80 }
  RangedAttack: { Energy: 100 }   # 伤害 25
  Heal:         { Energy: 250 }
  Claim:        { Energy: 600 }
  Tough:        { Energy: 10 }

# world.toml 可覆盖以上默认值，例如:
#   [actions.costs]
#   body_part.Move = { Energy: 60, Crystal: 10 }

host_functions:
  tick:
    export: true
    params: [snapshot_ptr: i32, snapshot_len: i32]
    returns: i32  # 0 = success, pointer to command JSON in WASM memory

  # 世界配置查询（只读）
  get_world_config:
    params: [key_ptr: i32, key_len: i32, out_ptr: i32, out_len: i32]
    returns: i32

  get_world_rules:
    params: [out_ptr: i32, out_len: i32]
    returns: i32

  # 地形与寻路查询（只读，计入 fuel）
  get_terrain:
    params: [x: i32, y: i32]
    returns: i32  # terrain_type as i32 (0=plain, 1=wall, 2=swamp, 3=lava)

  get_objects_in_range:
    params: [x: i32, y: i32, range: i32, out_ptr: i32, out_len: i32]
    returns: i32  # 写入 object_id 列表到 out_ptr
    limit: 5 calls/tick

  path_find:
    params: [from_x: i32, from_y: i32, to_x: i32, to_y: i32, out_ptr: i32, out_len: i32]
    returns: i32  # 写入路径坐标列表到 out_ptr
    limit: 10 calls/tick

global_storage_commands:
  TransferToGlobal:
    params: { resource: ResourceName, amount: ResourceAmount }
    validator: [global_storage_enabled, has_local_resource, under_capacity, transfer_time_remaining(0)]
    cost: registry.transfer_to_global_cost() * amount
    duration: transfer_to_global_time  # tick 数，运输期间资源不可用

  TransferFromGlobal:
    params: { resource: ResourceName, amount: ResourceAmount }
    validator: [global_storage_enabled, has_global_resource, transfer_time_remaining(0)]
    cost: registry.transfer_from_global_cost() * amount
    duration: transfer_from_global_time

refund_policy:
  contention_lost: 0.5    # SourceEmpty, TileOccupied, TargetFull
  self_invalid: 0.0       # OutOfRange, Fatigued, MissingBodyPart, etc.
```

## 3. 代码生成规则

| 目标 | 生成物 |
|------|--------|
| Rust | `src/generated/commands.rs` — Command enum + validate() |
| Rust | `src/generated/host_functions.rs` — host function stubs |
| TS SDK | `sdk-ts/src/generated/api.ts` — types + autocomplete |
| MCP | MCP tool schemas JSON |
| Replay | TickTrace schema — 冻结于 Phase 0；格式变更需递增 ABI 版本 |
| Docs | API reference markdown |

## 4. CI 检查

```bash
cargo run -- gen-api        # 从 IDL 生成代码
git diff --exit-code        # 生成代码与提交代码一致 → 不一致则 CI 失败
```

任何对游戏 API 的修改必须从 IDL 开始——修改 `game_api.idl` → 重新生成 → 提交生成的代码。不允许手写 Command 变体或 host function。
