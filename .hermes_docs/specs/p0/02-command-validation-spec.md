# P0-2: 指令校验规范

> **状态**: Frozen for Phase 0 | **实现阶段**: Phase 2

## 1. 指令管线

```
tick() 输出 JSON（来自 WASM 模块）
    │
    ▼
┌─────────────────┐
│  Tick 输出 Schema  │  JSON schema 验证：最大 256KB、拒绝额外字段、深度≤10
│  校验              │  超限/畸形的 tick 输出直接丢弃，不计入 refund
└────────┬────────┘
         │ Ok(Command[])
         ▼
┌─────────────────┐
│  反序列化         │  JSON 解析，逐指令 schema 验证，边界检查
└────────┬────────┘
         │ Ok(RawCommand[])
         ▼
┌─────────────────┐
│  预校验           │  静态检查：目标存在、归属匹配、距离范围内
└────────┬────────┘
         │ Ok(ValidatedCommand[])
         ▼
┌─────────────────┐
│  应用            │  修改世界状态（FDB 事务内）
└────────┬────────┘
         │ Ok / Err(RejectionReason)
         ▼
   记录到 TickTrace
```

**单一管线**：所有入口（WASM tick 输出、MCP tool、REST API、admin CLI）走同一 `校验 → 应用` 路径。无绕过。

### 1.1 Tick 输出 JSON Schema

WASM 模块的 `tick()` 必须返回符合以下 schema 的 JSON：

```json
{
  "$schema": "https://json-schema.org/draft/2020-12/schema",
  "type": "array",
  "maxItems": 100,
  "items": { "$ref": "#/definitions/Command" }
}
```

- 顶层必须是 JSON **数组**（非 object、非 null、非原始值）
- 数组长度 ≤ MAX_COMMANDS_PER_PLAYER (100)
- 总字节数 ≤ 256 KB
- `additionalProperties: false` — 拒绝未知顶层字段
- 深度限制 ≤ 10 层
- 包含非 JSON 字节序列（二进制垃圾）→ 校验失败，整个 tick 输出丢弃

> 校验失败的 tick 输出：不计入 refund（未进入指令管线），记录到 TickTrace 为 `TickValidationFailed`。

## 2. 指令类型层次

服务端指令管线处理三种不同的指令表示，从不可信输入逐步升级为可信的已验证指令：

```
CommandIntent (WASM 输出, 不可信)
    │  仅含 sequence + action 两个字段
    │  player_id / source / tick 全部由 Source Gate 服务端注入
    ▼
RawCommand (服务端 envelope, auth 已注入)
    │  player_id / tick / sequence / action + auth context
    │  通过 Source Gate 后进入校验管线
    ▼
ValidatedCommand (校验通过, 可安全执行)
    │  所有静态检查已通过
    │  携带解析后的目标引用、距离、成本等缓存数据
    ▼
  进入应用阶段（修改世界状态）
```

### 2.1 CommandIntent（不可信输入）

WASM 模块的 `tick()` 只输出 `CommandIntent[]`，**仅允许两个字段**：

```json
{
  "sequence": 3,
  "action": {
    "type": "Move",
    "object_id": 1001,
    "direction": "TopRight"
  }
}
```

| 字段 | 类型 | 说明 |
|------|------|------|
| `sequence` | u32 | 每 tick 单调递增，WASM 自行管理 |
| `action` | Action | 见 §3 逐指令校验矩阵 |

**禁止字段**：`player_id`、`source`、`tick`、`auth` 等字段**不得**由 WASM 提供。若 CommandIntent 包含这些字段 → 整个 tick 输出被拒绝（`TickValidationFailed`），不计入 refund。

### 2.2 RawCommand（服务端 envelope）

Source Gate 验证 CommandIntent 后，服务端注入身份与时序上下文，形成 RawCommand：

```json
{
  "player_id": 42,
  "tick": 4521,
  "sequence": 3,
  "source": "WASM",
  "action": {
    "type": "Move",
    "object_id": 1001,
    "direction": "TopRight"
  }
}
```

| 字段 | 类型 | 来源 | 校验规则 |
|------|------|------|---------|
| `player_id` | u32 | **服务端注入** | 必须匹配已认证玩家 |
| `tick` | u64 | **服务端注入** | 必须是当前 tick 或下一 tick（预提交） |
| `source` | Source | **服务端注入** | 见 P0-9 §2.1 来源矩阵 |
| `sequence` | u32 | WASM 提供 | 每玩家每 tick 单调递增 |
| `action` | Action | WASM 提供 | 见 §3 逐指令校验 |

### 2.3 ValidatedCommand（校验通过）

预校验阶段（§1 管线第3步）将 RawCommand 升级为 ValidatedCommand，携带解析后的引用：

```json
{
  "player_id": 42,
  "tick": 4521,
  "sequence": 3,
  "source": "WASM",
  "action_type": "Move",
  "resolved": {
    "object_ref": EntityRef(1001),
    "object_position": { "x": 5, "y": 3, "room": 1 },
    "target_ref": null,
    "distance_to_target": null,
    "cost": {}
  }
}
```

`resolved` 字段由预校验阶段填充，供应用阶段直接使用，避免二次查表。若预校验失败，返回 `RejectionReason`（见 §5）。

## 3. 逐指令校验矩阵

### 3.1 Move

```json
{"type": "Move", "object_id": 1001, "direction": "TopRight"}
```

| 检查项 | 失败码 |
|--------|--------|
| `object_id` 存在于世界中 | `ObjectNotFound` |
| `object_id.owner == player_id` | `NotOwner` |
| `object_id` 是 Drone（非 Structure/Resource） | `NotMovable` |
| `drone.fatigue == 0` | `Fatigued` |
| `drone.body` 包含 `Move` 部件 | `MissingBodyPart(Move)` |
| 目标格可通行（非 Wall、非敌对占据） | `TileBlocked` |
| Direction 是合法六边形邻居 | `InvalidDirection` |
| Drone 非 spawning 状态 | `StillSpawning` |

### 3.2 MoveTo

```json
{"type": "MoveTo", "object_id": 1001, "x": 15, "y": 22}
```

| 检查项 | 失败码 |
|--------|--------|
| 所有 Move 检查项 (3.1) 均适用 | (同 Move) |
| `(x, y)` 在当前房间内 | `OutOfRoom` |
| 从当前位置到 `(x, y)` 存在路径 | `NoPath` |
| 路径长度 ≤ MAX_PATH_LENGTH (100) | `PathTooLong` |
| `drone.body` 含 MOVE 部件数量 ≥ 路径长度 | `InsufficientMoveParts` |

### 3.3 Harvest

```json
{"type": "Harvest", "object_id": 1001, "target_id": 4001}
```

| 检查项 | 失败码 |
|--------|--------|
| `object_id` 是玩家拥有的 Drone | `NotOwner` |
| `drone.body` 包含 `Work` 部件 | `MissingBodyPart(Work)` |
| `drone.body` 包含 `Carry` 部件 | `MissingBodyPart(Carry)` |
| `drone.carry_used < drone.carry_capacity` | `CarryFull` |
| `target_id` 是 Source | `NotSource` |
| `target.source.energy > 0` | `SourceEmpty` |
| `object_id` 在 `target_id` 范围内 (range = 1) | `OutOfRange` |
| `drone.fatigue == 0` | `Fatigued` |

### 3.4 Transfer / Withdraw

```json
{"type": "Transfer", "object_id": 1001, "target_id": 2001, "resource": "Energy", "amount": 50}
{"type": "Withdraw", "object_id": 1001, "target_id": 2001, "resource": "Energy", "amount": 50}
```

| 检查项 | 失败码 |
|--------|--------|
| `object_id` 是玩家拥有的 Drone | `NotOwner` |
| `drone.body` 包含 `Carry` 部件 | `MissingBodyPart(Carry)` |
| Transfer: `drone.carry[resource] >= amount` | `InsufficientResources` |
| Withdraw: `target.carry[resource] >= amount` | `InsufficientResources` |
| 目标有该资源的容量 | `TargetFull` / `TargetEmpty` |
| `object_id` 在范围内 (range = 1) | `OutOfRange` |

### 3.5 Build

```json
{"type": "Build", "object_id": 1001, "x": 10, "y": 15, "structure": "Extension"}
```

| 检查项 | 失败码 |
|--------|--------|
| `object_id` 是玩家拥有的 Drone | `NotOwner` |
| `drone.body` 包含 `Work` + `Carry` 部件 | `MissingBodyPart` |
| `drone.carry[Energy] >= build_cost(structure)` | `InsufficientEnergy` |
| `(x, y)` 在玩家拥有 Controller 的房间 | `NotYourRoom` |
| 该格为空（无既有建筑） | `TileOccupied` |
| 该格是 Plain 地形 | `InvalidTerrain` |
| 在建工程数 < MAX_CONSTRUCTION_SITES (100) | `TooManyConstructionSites` |
| `object_id` 在 `(x, y)` 范围内 (range = 3) | `OutOfRange` |

### 3.6 Repair

```json
{"type": "Repair", "object_id": 1001, "target_id": 2002}
```

| 检查项 | 失败码 |
|--------|--------|
| `object_id` 是带 Work+Carry 的 Drone | `MissingBodyPart` |
| `target_id` 是 Structure | `NotStructure` |
| `target.hits < target.hits_max` | `AlreadyFullHealth` |
| `drone.carry[Energy] >= repair_cost` | `InsufficientEnergy` |
| `object_id` 在范围内 (range = 3) | `OutOfRange` |

### 3.7 Attack

```json
{"type": "Attack", "object_id": 1001, "target_id": 1002}
```

| 检查项 | 失败码 |
|--------|--------|
| `object_id` 是玩家拥有的 Drone | `NotOwner` |
| `drone.body` 包含 `Attack` 部件 | `MissingBodyPart(Attack)` |
| `target_id` 存在 | `ObjectNotFound` |
| `target_id.owner != player_id` 或为中立敌对 | `FriendlyTarget` |
| `object_id` 在范围内 (range = 1) | `OutOfRange` |
| `drone.fatigue == 0` | `Fatigued` |

**TOCTOU**: 如果目标在快照和执行之间移动了，按当前位置检查范围 → 移开则 `OutOfRange`。攻击不跟踪移动目标。

### 3.8 RangedAttack

与 Attack 相同，range = 3，需要 `RangedAttack` 身体部件。

### 3.9 Heal

```json
{"type": "Heal", "object_id": 1001, "target_id": 1003}
```

| 检查项 | 失败码 |
|--------|--------|
| `drone.body` 包含 `Heal` 部件 | `MissingBodyPart(Heal)` |
| `target.hits < target.hits_max` | `AlreadyFullHealth` |
| 目标属于玩家或盟友 | `NotFriendly` |
| Range = 3 | `OutOfRange` |

### 3.10 Spawn

```json
{"type": "Spawn", "spawn_id": 2001, "body": ["Move", "Work", "Carry", "Move"]}
```

| 检查项 | 失败码 |
|--------|--------|
| `spawn_id` 是玩家拥有的 Spawn | `NotYourSpawn` |
| `spawn.cooldown == 0` | `SpawnOnCooldown` |
| `body.len() ≤ MAX_BODY_PARTS (50)` | `BodyTooLarge` |
| `body_cost(body) ≤ spawn.energy` | `InsufficientEnergy` |
| `body_cost(body) ≤ 玩家房间能量上限` | `ExceedsRoomCapacity` |
| 房间有空余 spawn 槽位 | `RoomDroneCapReached` |

Drone 在 tick 末尾创建（death_cleanup_system 之后，spawn 槽位已释放）。

### 3.11 Recycle

```json
{"type": "Recycle", "object_id": 1001, "spawn_id": 2001}
```

| 检查项 | 失败码 |
|--------|--------|
| `object_id` 是玩家拥有的 Drone | `NotOwner` |
| `spawn_id` 是玩家的 Spawn | `NotYourSpawn` |
| `object_id` 在 spawn 范围内 (range = 1) | `OutOfRange` |

返还 50% 身体部件成本作为能量给 spawn。

### 3.12 Hack（特殊攻击）

```json
{"type": "Hack", "object_id": 1001, "target_id": 1002}
```

| 检查项 | 失败码 |
|--------|--------|
| `object_id` 是玩家拥有的 Drone | `NotOwner` |
| `drone.body` 包含 `Claim` 部件 | `MissingBodyPart(Claim)` |
| `target_id` 存在且是 Drone | `ObjectNotFound` |
| `target_id.owner != player_id`（非己方） | `FriendlyTarget` |
| `object_id` 在范围内 (range = 1) | `OutOfRange` |
| `drone.fatigue == 0` | `Fatigued` |
| 冷却未到（200 tick，全局） | `OnCooldown` |
| 目标未被其他玩家 Hack 中 | `AlreadyHacked` |

**效果**: 施加"控制锁"逐步建立控制——tick 1-2 目标减速 50%，tick 3-4 目标无法移动，tick 5 夺取成功（drone 转为 Neutral，停止执行 WASM，进入 idle）。5 tick 后自动恢复。idle 期间不消耗 lifespan。目标可通过 Disrupt 打断或 Fortify 净化控制锁。

**状态转换**: Hack 成功 → 目标 drone 获得 `HackControlLock{stage: 1-5}` 状态，每 tick stage 递增。stage=5 时 drone 转为 Neutral（`owner=0`，不执行 WASM，不消耗 fuel/lifespan）。5 tick 后自动恢复原 owner。Neutral 期间免疫再次 Hack。

**冷却**: 200 tick（全局冷却）。**资源消耗**: 1000 Energy。**抗性**: 目标 `Psionic` 抗性影响成功率。

### 3.13 Drain（特殊攻击）

```json
{"type": "Drain", "object_id": 1001, "target_id": 2002, "resource": "Energy"}
```

| 检查项 | 失败码 |
|--------|--------|
| `object_id` 是玩家拥有的 Drone | `NotOwner` |
| `drone.body` 包含 `Work` + `Carry` 部件 | `MissingBodyPart` |
| `target_id` 是 Structure 或 Storage | `NotStructure` |
| `target_id.owner != player_id`（非己方） | `FriendlyTarget` |
| `target` 有指定 resource 存量 > 0 | `TargetEmpty` |
| `drone.carry_used < drone.carry_capacity` | `CarryFull` |
| `object_id` 在范围内 (range = 1) | `OutOfRange` |
| `drone.fatigue == 0` | `Fatigued` |
| 冷却未到（50 tick，每 drone） | `OnCooldown` |

**效果**: 从目标建筑/存储中窃取资源，每 tick 转移 `carry_capacity` 单位。

**状态转换**: Drain 成功 → 开始持续窃取。持续时间：drone 保持范围内则持续。移动或被 Disrupt → 中断。

**冷却**: 50 tick（每 drone）。**资源消耗**: 200 Energy/tick。**抗性**: 目标 `EMP` 抗性影响窃取效率。

### 3.14 Overload（特殊攻击）

```json
{"type": "Overload", "object_id": 1001, "target_id": 42}
```

| 检查项 | 失败码 |
|--------|--------|
| `object_id` 是玩家拥有的 Drone | `NotOwner` |
| `drone.body` 包含 `RangedAttack` 部件 | `MissingBodyPart(RangedAttack)` |
| `target_id` 是有效的 player_id | `PlayerNotFound` |
| `target_id != player_id`（非己方） | `FriendlyTarget` |
| `target_player.fuel_budget > MAX_FUEL × 0.2` | `TargetFuelTooLow` |
| `drone.fatigue == 0` | `Fatigued` |
| 冷却未到（200 tick，每 drone） | `OnCooldown` |

**效果**: 消耗目标计算配额。目标 `fuel budget` 减少 500k（默认 MAX_FUEL=10M 的 5%）。**下限 MAX_FUEL × 0.2**（不可降至更低）。无 range 限制——Overload 是逻辑攻击。

**冷却**: 200 tick（每 drone）。**资源消耗**: 300 Energy。**抗性**: 目标 `EMP` 抗性影响削减量。

### 3.15 Debilitate（特殊攻击）

```json
{"type": "Debilitate", "object_id": 1001, "target_id": 1003, "damage_type": "EMP"}
```

| 检查项 | 失败码 |
|--------|--------|
| `object_id` 是玩家拥有的 Drone | `NotOwner` |
| `drone.body` 包含 `Work` 部件 | `MissingBodyPart(Work)` |
| `target_id` 存在（Drone 或 Structure） | `ObjectNotFound` |
| `target_id.owner != player_id`（非己方） | `FriendlyTarget` |
| `damage_type` ∈ DamageType 枚举 | `InvalidDamageType` |
| `target` 未被同类型 Debilitate 叠加 | `AlreadyDebilitated(damage_type)` |
| `object_id` 在范围内 (range = 3) | `OutOfRange` |
| `drone.fatigue == 0` | `Fatigued` |
| 冷却未到（150 tick，每 drone） | `OnCooldown` |

**效果**: 给目标附加易伤状态。指定伤害类型抗性 ×2（受到该类型伤害加倍），持续 50 tick。

**状态转换**: Debilitate 成功 → 目标获得 `Debilitated{damage_type}` 状态（duration=50 tick）。同一目标可同时有不同类型的 Debilitate，但同类型不可叠加。

**冷却**: 150 tick（每 drone）。**资源消耗**: 200 Energy。**抗性**: 目标 `Corrosive` 抗性影响成功率。

### 3.16 Disrupt（特殊攻击）

```json
{"type": "Disrupt", "object_id": 1001, "target_id": 1002}
```

| 检查项 | 失败码 |
|--------|--------|
| `object_id` 是玩家拥有的 Drone | `NotOwner` |
| `drone.body` 包含 `Attack` 部件 | `MissingBodyPart(Attack)` |
| `target_id` 存在且是 Drone | `ObjectNotFound` |
| `target_id.owner != player_id`（非己方） | `FriendlyTarget` |
| `object_id` 在范围内 (range = 1) | `OutOfRange` |
| `drone.fatigue == 0` | `Fatigued` |
| 冷却未到（50 tick，每 drone） | `OnCooldown` |

**效果**: 打断目标当前持续动作（Drain/Hack 控制锁等立即终止）。不造成 HP 伤害。

**冷却**: 50 tick（每 drone）。**资源消耗**: 100 Energy。**抗性**: 目标 `Sonic` 抗性影响成功率。

### 3.17 Fortify（特殊攻击/防御）

```json
{"type": "Fortify", "object_id": 1001, "target_id": 1003}
```

| 检查项 | 失败码 |
|--------|--------|
| `object_id` 是玩家拥有的 Drone | `NotOwner` |
| `drone.body` 包含 `Tough` 部件 | `MissingBodyPart(Tough)` |
| `target_id` 存在（Drone 或 Structure） | `ObjectNotFound` |
| `target_id.owner == player_id` 或为盟友 | `NotFriendly` |
| `object_id` 在范围内 (range = 1) | `OutOfRange` |
| `drone.fatigue == 0` | `Fatigued` |
| 冷却未到（300 tick，每 drone） | `OnCooldown` |

若 `target_id` 省略，默认 fortify 自身（`object_id`）。

**效果**: 自身/友方获得护盾（所有抗性 ×0.5，即伤害减半）。**同时清除目标所有负面状态**（Debilitate/Drain/Overload/Hack 控制锁），持续 100 tick。

**冷却**: 300 tick（每 drone）。**资源消耗**: 400 Energy。**抗性**: 无——这是增益+净化，不受抗性影响。

**注意**: §3.12-3.17 特殊攻击为 Phase 6 实现内容。IDL 定义见 P0-8。

## 4. 查询指令（只读）

查询不进指令管线。它们在快照生成阶段（阶段一）处理。

### 4.1 GetTerrain

返回 (x, y) 处地形类型。纯服务端操作。不计每 tick 配额——静态数据。

### 4.2 GetObjectsInRange

返回 (x, y) 周围 `range` 内的可见实体。
- `range ≤ MAX_QUERY_RANGE (10)`
- 仅返回玩家可见的实体（遵循 fog-of-war）
- 每玩家每 tick 查询配额：5 次

### 4.3 PathFind

返回 (from_x, from_y) 到 (to_x, to_y) 的最优路径。
- 两点在同一房间内
- `path_length ≤ MAX_PATH_LENGTH (100)` — 超出则中止
- 计入玩家计算预算（WASM fuel 或 MCP 查询配额）
- 每玩家每 tick：10 次
- 结果以 `(from, to, 地形hash)` 缓存 — 地形不变不重算

## 5. 拒绝响应

每次拒绝返回：

```json
{
  "command": { /* 原始 RawCommand */ },
  "rejection": "OutOfRange",
  "detail": "object_1001 at (5,3), target_1002 at (5,6) — distance 3, require ≤ 1",
  "tick": 4521
}
```

`detail` 字段是机器可读 JSON，含精确位置、距离和阈值。后续可基于此生成 UX 友好的解释（见 P0-6）。

## 6. 硬性边界与限制

| 参数 | 限值 | 原因 |
|------|------|------|
| MAX_BODY_PARTS | 50 | 防止 spawn 向量膨胀攻击 |
| MAX_PATH_LENGTH | 100 | 防止寻路计算爆炸 |
| MAX_QUERY_RANGE | 10 | 防止范围扫描过广 |
| MAX_COMMANDS_PER_PLAYER | 100/tick | 限制 MCP 工具滥用 |
| MAX_CONSTRUCTION_SITES | 100/房间 | 防止建造刷屏 |
| MAX_DRONES_PER_PLAYER | 500 | 可配置（world.toml 中 `drone.max_drones_per_player`），默认 500 |
| 玩家名称 | 32 字符, `[a-zA-Z0-9 _-]` | 防 prompt 注入。**Prompt injection delimiter 必须使用此字符集之外的字符**（如 `[[`/`]]` 或 Unicode），确保玩家名无法伪造系统与用户内容的边界。 |
| 房间名称 | 16 字符, `[A-Z][0-9]+[NS][0-9]+[EW]` | 标准化格式 |
| JSON 深度 | 10 | serde_json 递归限制 |
| 字符串最大长度（通用） | 256 字符 | 通用保护 |
| i32 坐标范围 | [-128, 127] 每房间 | 防止溢出攻击 |

## 7. 资源争用 Refund 策略

### 7.1 退还规则

| 拒绝原因 | Refund | 理由 |
|---------|--------|------|
| `SourceEmpty` | 退 50% fuel | 竞争导致——非玩家过错 |
| `TileOccupied` | 退 50% fuel | 同上 |
| `TargetFull` | 退 50% fuel | 同上 |
| `OutOfRange` | 不退 | 玩家应检查距离 |
| `Fatigued` | 不退 | 玩家应检查疲劳 |
| `MissingBodyPart` | 不退 | 玩家应知道自己 drone 组成 |
| `InsufficientResource` | 不退 | 玩家应计算资源 |
| `ObjectNotFound` | 不退 | 目标已被销毁——信息过期 |
| 其他所有 | 不退 | 默认不退款 |

### 7.2 退还时序（Anti-Amplification）

**退还的 fuel 仅作用于下一 tick 的 fuel budget**，禁止同 tick 内计算放大：

- tick N 的指令在 tick N 执行阶段被拒绝 → 退还 credit 记入玩家的 `next_tick_fuel_credit`
- tick N+1 开始时，玩家 fuel budget = `MAX_FUEL + next_tick_fuel_credit`（不超过 `MAX_FUEL × 1.1`）
- 同 tick 内不得通过故意竞争失败来获取额外计算预算
- **Deploy-reset 规则**: refund credit 与玩家绑定。若玩家在 tick N+1 执行了任何部署操作（`swarm_deploy` / `MCP_Deploy` / `Deploy`），tick N 及之前累计的 refund credit 清零。防止 v1 刷 refund → v2 消费的跨模块预算转移。**例外**: 同一 session 内的迭代部署（同 session_id）不清除 credit——不惩罚正常迭代。

### 7.3 退还上限与滥用检测

| 限制 | 值 | 说明 |
|------|------|------|
| 每人每 tick 退还上限 | `MAX_FUEL × 10%` | 当前为 1,000,000 fuel 上限 |
| 同源重复失败 | 仅首次退 50%，后续 0% | 同一 `(player, source, rejection_reason)` 在同一 tick 内重复退还不累计 |
| 连续高退还率 throttle | 退还率 > 80% 连续 3 tick | 触发 throttle：该玩家下一 tick fuel budget 降为 `MAX_FUEL × 0.5` |

### 7.4 监控指标

| 指标 | 阈值 | 动作 |
|------|------|------|
| `refund_abuse_rate` | 退还 fuel / 总消耗 fuel > 0.5 | 记录到审计日志 |
| `source_empty_refund_pct` | SourceEmpty 占总退还 > 80% | 标记为可疑行为模式 |
| `consecutive_high_refund_ticks` | ≥ 3 | 自动 throttle（见上表） |
