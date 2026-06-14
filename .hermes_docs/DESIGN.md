# Swarm — 设计文档

> **Swarm** 是一个开源的、可编程的 MMO RTS 游戏引擎。它是 [Screeps](https://screeps.com/) 的精神续作，用现代技术栈从零重构，支持多语言。
>
> — *「你的代码就是你的军队。Write once, fight forever.」*

---

## 1. 愿景

### 1.1 核心理念

Swarm 是一个**编程竞技场**——玩家编写真实代码来控制自主单位（drone），在一个持久共享世界中运行。与传统 RTS 不同，Swarm 的胜负不取决于手速，而取决于**算法思维、系统设计和资源优化**。

Swarm 支持两种玩家：
- **人类程序员**：通过 Web UI（Monaco 编辑器 + PixiJS 渲染）编写代码，编译为 WASM 部署
- **AI agent**：通过 MCP 接口查看世界、生成代码、部署 WASM——与人类走完全相同路径

世界只认 WASM。不论代码是谁写的。

### 1.2 与 Screeps 的关键区别

| 维度 | Screeps | Swarm |
|------|---------|-------|
| **玩家语言** | 仅 JavaScript | **任意语言 → WASM** |
| **沙箱** | V8 Isolate (isolated-vm) | WASM + WASI（Wasmtime，独立进程隔离） |
| **资源计量** | 墙钟 CPU 限制 | **CPU 指令计数**（fuel metering） |
| **游戏模型** | OOP 脚本模式 | **ECS**（Bevy）— 确定性、可并行、可回放 |
| **性能** | 受限于 V8 GC | WASM 原生速度，同等配额快 10-100 倍 |
| **AI 玩家** | 无原生支持 | **MCP 原生界面**——AI 写 WASM，同人类 |
| **扩展性** | 仅 JS mod | WASM 多语言 SDK + 插件系统 |
| **客户端** | Web + Steam 封装浏览器 | Web（Monaco + PixiJS）+ MCP（AI 界面） |

### 1.3 设计原则

1. **语言无关**：引擎不知道也不关心玩家代码是什么语言写的。一切编译为 WASM。
2. **确定性核心**：相同初始状态 + 相同玩家指令 → 相同世界状态。支撑回放、调试和反作弊。
3. **公平资源核算**：CPU 配额度量为 WASM 指令数，非墙钟。C 玩家和 Python 玩家在相同配额下获得同等算力。AI 玩家和人类玩家同走 WASM 沙箱，天然公平。
4. **可组合架构**：ECS 允许新增游戏机制时无需触碰既有代码。
5. **开源首日**：MIT 许可证。

---

## 2. 系统架构

### 2.1 整体架构图

```
┌──────────────────────────────────────────────────────────┐
│                        客户端                               │
│  ┌─────────────┐  ┌──────────────┐  ┌─────────────────┐ │
│  │ Web Client   │  │ Desktop App   │  │ CI/CD Pipeline   │ │
│  │ (Monaco +    │  │ (Tauri)       │  │ (GitHub Actions) │ │
│  │  PixiJS)     │  │               │  │                  │ │
│  └──────┬───────┘  └──────┬───────┘  └────────┬────────┘ │
│         │                 │                    │           │
│  ┌──────┴─────────────────┴────────────────────┴────────┐ │
│  │                   MCP Interface (AI 玩家)              │ │
│  │  AI agent 查看世界 · 生成代码 · 部署 WASM · 调试       │ │
│  └────────────────────────┬─────────────────────────────┘ │
└───────────────────────────┼───────────────────────────────┘
                            │
            WebSocket + REST │ HTTPS (MCP)
                            ▼
┌─────────────────────────────────────────────────────────┐
│                   网关 (Go)                                │
│  ┌──────────────┐  ┌─────────────┐  ┌───────────────┐  │
│  │ WS Hub        │  │ Auth (OAuth) │  │ API Router     │  │
│  └──────────────┘  └─────────────┘  └───────────────┘  │
└────────────────────────┬────────────────────────────────┘
                         │ gRPC + NATS
                         ▼
┌─────────────────────────────────────────────────────────┐
│                Tick 引擎 (Rust)                            │
│                                                           │
│  ┌──────────────────────────────────────────────────┐   │
│  │              Tick 调度器                            │   │
│  │  Tick N-1 完成 → Tick N 分发 → Tick N+1           │   │
│  └──────────────────────┬───────────────────────────┘   │
│                         │                                 │
│          ┌──────────────┼──────────────┐                 │
│          ▼              ▼              ▼                  │
│  ┌─────────────┐ ┌────────────┐ ┌──────────────┐        │
│  │ Sandbox     │ │ Sandbox    │ │ Sandbox      │  ...   │
│  │ Worker 1    │ │ Worker 2   │ │ Worker 3     │        │
│  │ (独立进程)   │ │ (独立进程)  │ │ (独立进程)    │        │
│  │ WASM 玩家 1 │ │ WASM 玩家2 │ │ WASM 玩家 3  │        │
│  └──────┬──────┘ └─────┬──────┘ └──────┬───────┘        │
│         │              │               │                 │
│         ▼              ▼               ▼                 │
│  ┌──────────────────────────────────────────────────┐   │
│  │         指令收集器 + 校验器 + 反作弊               │   │
│  │    (去重、冲突解决、反作弊)                        │   │
│  └──────────────────────┬───────────────────────────┘   │
│                         │                                 │
│  ┌──────────────────────▼───────────────────────────┐   │
│  │              Bevy ECS 世界                         │   │
│  │  ┌────────┐ ┌────────┐ ┌────────┐ ┌───────────┐  │   │
│  │  │ 移动   │ │ 战斗   │ │ 经济   │ │ 建造      │  │   │
│  │  │ System │ │ System │ │ System │ │ System    │  │   │
│  │  └────────┘ └────────┘ └────────┘ └───────────┘  │   │
│  │  ┌────────┐ ┌────────┐ ┌────────┐ ┌───────────┐  │   │
│  │  │ 视野   │ │ 资源   │ │ 寻路   │ │ 死亡      │  │   │
│  │  │ System │ │ System │ │ System │ │ System    │  │   │
│  │  └────────┘ └────────┘ └────────┘ └───────────┘  │   │
│  └──────────────────────────────────────────────────┘   │
│                                                           │
│  ┌───────────────────┐  ┌───────────────┐                 │
│  │ MCP Server        │  │ Debug/Trace   │                 │
│  │ (rmcp, HTTP/SSE)  │  │ Collector     │                 │
│  └───────────────────┘  └───────────────┘                 │
└────────────────────────┬────────────────────────────────┘
                         │
                         ▼
┌─────────────────────────────────────────────────────────┐
│                   数据层                                   │
│  ┌──────────────┐  ┌─────────────┐  ┌───────────────┐  │
│  │ FoundationDB  │  │ Dragonfly    │  │ ClickHouse     │  │
│  │ (世界状态)    │  │ (热缓存)     │  │ (分析 + 审计)  │  │
│  └──────────────┘  └─────────────┘  └───────────────┘  │
└─────────────────────────────────────────────────────────┘
```

### 2.2 仓库结构

```
swarm/
├── docs/           # 设计文档、P0 规范、评审报告
│   ├── design/     #   架构设计
│   ├── specs/      #   技术规范
│   └── reviews/    #   评审报告
├── engine/         # Rust 游戏引擎 — Bevy ECS, Tick 调度, 世界模拟
├── sandbox/        # WASM 沙箱运行时 — 编译服务, 模块管理, 安全审计
├── gateway/        # Go API 网关 — WebSocket, REST, gRPC, 认证
├── frontend/       # Web 客户端 — Monaco Editor, PixiJS 渲染
├── sdk-ts/         # TypeScript SDK — 游戏 API 类型 + WASM 编译工具链
└── sdk-rust/       # Rust SDK — 游戏 API + wasm-bindgen 工具链
```

---

## 3. Engine（Rust）

**技术栈**：Rust + Bevy ECS + Tokio + FoundationDB

### 3.1 核心 ECS 实体

```rust
// 位置——所有有位置的实体都有此组件
struct Position { x: i32, y: i32, room: RoomId }

// 所有权
struct Owner(PlayerId);

// Drone——玩家的可编程单位
struct Drone {
    owner: PlayerId,
    body: Vec<BodyPart>,       // MOVE, WORK, CARRY, ATTACK 等
    fatigue: u32,              // 疲劳值，0 才能行动
    hits: u32,
    hits_max: u32,
    spawning: bool,
    age: u32,                  // 创建后经过的 tick 数。达到 lifespan 后死亡
}

/// drone 生命周期 — 年龄达到上限后自动死亡。
/// 默认值 1500 tick，可通过 world.toml `drone.lifespan` 覆盖。
const DEFAULT_DRONE_LIFESPAN: u32 = 1500;

// Structure——建筑
struct Structure {
    structure_type: StructureType,  // Spawn, Extension, Tower, Storage 等
    owner: Option<PlayerId>,
    hits: u32, hits_max: u32,
    energy: Option<u32>,
    energy_capacity: Option<u32>,
    cooldown: u32,
}

// Resource——掉落资源（动态资源类型）
struct Resource {
    amounts: IndexMap<String, u32>,    // IndexMap 保证迭代顺序确定。{ "Energy": 500, "Matter": 200 }
}

// Source——可再生资源点
struct Source {
    produces: IndexMap<String, u32>,   // IndexMap 保证迭代顺序确定。{ "Energy": 1 } 或 { "Energy": 1, "Matter": 1 }
    capacity: u32,
    ticks_to_regeneration: u32,
}

// Terrain——地形
struct Terrain(TerrainType);  // Plain, Swamp, Wall

// Controller——房间控制器（占领/升级）
struct Controller {
    owner: Option<PlayerId>,
    level: u8,                    // 1–8，控制可用建筑
    progress: u32, progress_total: u32,  // 升级进度
    downgrade_timer: u32,         // 降级倒计时（无 owner 时递减）
    safe_mode: u32,               // 安全模式剩余 tick
    safe_mode_available: u32,     // 可用安全模式次数
    safe_mode_cooldown: u32,      // 安全模式冷却
}
```

#### Controller 升级表 (RCL)

| Level | 累计 progress | 解锁建筑 | 最大房间 drone | 说明 |
|-------|-------------|---------|---------------|------|
| 1 | 0 | Spawn | 50 | 初始状态，仅能 spawn drone |
| 2 | 200 | Extension (5), Road, Container | 100 | 开始储能，物流起步 |
| 3 | 500 | Extension (10), Tower, Storage | 200 | 防御可用，有仓库 |
| 4 | 1,500 | Extension (20), Link | 300 | 能源网络 |
| 5 | 5,000 | Extension (30), Terminal, Observer | 400 | 市场交易，视野 |
| 6 | 15,000 | Extension (40), Extractor, Lab, Factory | 500 | 自定义资源，制造 |
| 7 | 50,000 | Extension (50), PowerSpawn | 500（硬上限） | 晚期产能 |
| 8 | 150,000 | Extension (60), Nuker | 500 | 终极武器 |

**升级机制**: 在 Controller 所在房间内向 Controller 存入资源（通过 Transfer 指令），每 tick 自动转换为 `progress`。`progress >= progress_total` 时升级到下一级。

**降级**: 若 Controller 失去 owner 超过 `downgrade_timer`（默认 5000 tick），降一级，`progress` 重置为 0。
```

### 3.2 Tick 生命周期

```
每 tick（目标 3s）：

阶段一：收集 (COLLECT) — 并行, ~2.5s
  ├── 对每个活跃玩家:
  │   ├── 加载玩家 WASM 模块（缓存在内存中）
  │   ├── 序列化可见世界状态 → JSON 快照
  │   ├── 在 sandbox worker 进程中实例化 WASM，fuel limit = 玩家 CPU 配额
  │   ├── 调用 tick(snapshot) → 收集 Vec<Command>
  │   └── 过滤无效指令（超配额、非法操作）
  └── 收集全部指令到指令队列

阶段二：执行 (EXECUTE) — 串行, ~0.5s
  ├── 玩家顺序种子洗牌（seed = hash(tick_number, world_seed)）
  ├── Phase 2a: 命令循环（逐条 inline 应用）
  │   ├── 对每条指令（按洗牌后顺序 + 玩家内 sequence 排序）:
  │   │   ├── 对照**当前** Bevy World 状态校验（非快照）
  │   │   ├── 合法 → 立即通过对应 ECS system 应用变更
  │   │   ├── 资源竞争 → 先到先得（先执行者优先）
  │   │   └── 冲突 → 丢弃 + 记录 RejectionReason
  │   └── Spawn 命令在 Phase 2a 中只校验不入队
  ├── Phase 2b: ECS Systems 统一运行（`.chain()`）
  │   ├── death_mark_system（标记待死亡 entity，释放 room cap 槽位）
  │   ├── spawn_system（统一创建 Phase 2a 校验通过的 drone）
  │   ├── combat_system（damage 先 → heal 后，同 tick 内结算）
  │   └── regeneration/decay/death_cleanup/其他被动 systems
  ├── FDB 原子提交（全或无）
  └── tick_counter 推进

阶段三：广播 (BROADCAST) — 即时
  ├── 计算增量（与上一 tick 快照的实体差异）
  ├── Dragonfly 缓存更新
  ├── 通过 NATS → Gateway → WebSocket 客户端发布
  └── 每隔 N tick 记录完整世界快照到 FDB（回放用）
```

### 3.3 确定性保证

```
确定性需要：
1. 相同的初始世界状态
2. 相同的 Command 输入（已排序）
3. ECS System 执行顺序固定（.chain()）
4. 所有随机数来自确定种子 PRNG（不用 OS 熵源）

反作弊：
- 全量回放：任意房间状态可完整重现
- 异常检测：玩家 tick 间的世界变化超过物理上限 → 标记
- WASM 编译时静态分析：扫描可疑系统调用
```

---

## 4. MCP 接口——AI 玩家的操作界面

MCP 是 AI agent 的「屏幕和鼠标」——与人类玩家的 Web UI 完全同级。

```
人类：Monaco 编辑器 → 编译 WASM → 上传 ─┐
                                       ├─→ WasmSandboxExecutor → 世界
AI：  MCP 看世界 → 生成 WASM → 部署 ───┘
```

### 4.1 MCP 工具分类

| 类别 | 工具 | 用途 |
|------|------|------|
| **世界查看** | `swarm_get_snapshot` | 获取可见世界状态 |
| | `swarm_get_terrain` | 查看地形 |
| | `swarm_get_objects_in_range` | 查看范围内的实体 |
| **部署** | `swarm_deploy` | 上传 WASM 模块 |
| | `swarm_validate_module` | 上传前预检 |
| | `swarm_rollback` | 回滚到之前版本 |
| **调试** | `swarm_explain_last_tick` | 解释上 tick 发生了什么 |
| | `swarm_inspect_entity` | 检查实体完整状态 |
| | `swarm_profile` | 策略性能指标 |
| **学习** | `swarm_get_docs` | API 参考和游戏规则 |
| | `swarm_get_schema` | 游戏 API JSON Schema |
| | `swarm_get_available_actions` | 当前可用的 API 函数 |

### 4.2 明确不在 MCP 中

MCP 不做游戏动作。不存在 `swarm_move`、`swarm_attack`、`swarm_build` 等工具。AI agent 必须**编写 WASM 代码**来实现策略，和人类玩家完全一样。

详见 `specs/p0/03-mcp-security-contract.md`。

---

## 5. 游戏 API（Deferred Command Model）

WASM 模块通过 **deferred command model** 与引擎交互：

```
tick(snapshot_json) → Command[]
```

1. 引擎将快照 JSON 写入 WASM 线性内存
2. 调用 `tick(ptr, len)` — WASM 模块接收快照，返回指令 JSON 列表
3. 引擎校验所有指令 → 通过 P0-2 Command Validation Pipeline → 应用到世界

### 5.1 允许的 Host Function（查询专用，只读）

WASM 中**仅可调用查询类 host function**——所有函数只读，不计入指令预算但计入 fuel 预算：

```rust
// 信息查询（只读，不改变世界状态）
fn host_get_terrain(x: i32, y: i32) -> i32;
fn host_get_objects_in_range(x: i32, y: i32, range: i32, out_ptr: i32, out_len: i32) -> i32;
fn host_path_find(from_x: i32, from_y: i32, to_x: i32, to_y: i32, out_ptr: i32, out_len: i32) -> i32;

// 世界配置查询
fn host_get_world_config(key_ptr: i32, key_len: i32, out_ptr: i32, out_len: i32) -> i32;
fn host_get_world_rules(out_ptr: i32, out_len: i32) -> i32;
```

全部返回 `i32`：0 = 成功，负数 = 错误码。
`out_ptr`/`out_len`：WASM 分配缓冲区，host 写入结果后再次校验边界。

### 5.2 禁止的 Host Function

以下**游戏动作不得作为 host function 暴露给 WASM**。所有 mutating 操作通过 `tick() → Command[]` JSON 延迟模型提交，引擎在校验后统一应用：

- ❌ `host_move` / `host_move_to` — 改为 `{ "action": "Move", ... }` JSON 指令
- ❌ `host_harvest` / `host_transfer` / `host_withdraw`
- ❌ `host_build` / `host_repair`
- ❌ `host_attack` / `host_ranged_attack` / `host_heal`
- ❌ `host_spawn` / `host_recycle`

> **设计合同**: WASM 模块不直接调用 mutating host function。所有状态变更通过 `tick() → JSON` 延迟模型提交。完整 IDL 见 P0-8。

---

## 6. 数据模型

### 6.1 FoundationDB — 世界状态

```
/tick/{N}/state          → tick N 后的完整世界状态
/tick/{N}/commands       → 全部玩家的排序指令
/tick/{N}/rejections     → 被拒绝的指令及原因
/tick/{N}/metrics        → tick 指标
/player/{id}/profile     → 玩家档案
/player/{id}/modules/    → WASM 模块历史
```

### 6.2 Dragonfly — 热缓存

- 当前 tick 世界状态快照（高频读取）
- 玩家 session 映射（WS 连接 → player_id）
- 排行榜缓存（每分钟刷新）
- Rate limiting 计数器

### 6.3 ClickHouse — 分析

```sql
-- tick 指标
tick_metrics:    tick, player_id, cpu_fuel, cmd_count, cmd_success, latency_ms

-- MCP 审计
mcp_audit:       timestamp, player_id, tool_name, parameters, result

-- 游戏事件
player_events:   tick, player_id, event_type, entity_id, detail
```

---

## 7. 部署架构

### 7.1 开发环境（docker-compose）

```yaml
services:
  fdb:          # FoundationDB
  dragonfly:    # Redis 兼容缓存
  engine:       # Rust 引擎
  gateway:      # Go 网关
  frontend:     # Vite dev server
```

### 7.2 生产环境

> **Sharding 延期声明**: Phase 1-6 采用单进程部署（一个 Engine 实例处理所有房间）。DESIGN §3.2 的全局原子 tick 提交在单进程下自洽。Phase 7 引入多房间 sharding 时将重新设计提交粒度与跨 shard 可见性同步机制。当前文档中 `每 shard 一个实例` 的声明为 Phase 7 预留方向，非当前实现合同。

```
┌──────────────────────────────────────────────┐
│              负载均衡 (nginx / Traefik)         │
└──────┬───────────────────────┬───────────────┘
       │                       │
       ▼                       ▼
┌──────────────┐      ┌──────────────┐
│ Gateway-1    │      │ Gateway-2    │ ...  (Go, 无状态, 水平扩展)
└──────┬───────┘      └──────┬───────┘
       │                     │
       └──────────┬──────────┘
                  ▼
┌─────────────────────────────────┐
│         NATS 集群                │
└──────────┬──────────────────────┘
           │
           ▼
┌─────────────────────────────────┐
│     Engine (Rust)                │
│     (每 shard 一个实例)           │
└──────────┬──────────────────────┘
           │
           ▼
┌─────────────────────────────────┐
│   FoundationDB 集群               │
└─────────────────────────────────┘
```

---

## 8. World Rules Engine — 可配置的游戏规则

Swarm 不是「一个游戏」，而是「一个可配置的游戏引擎平台」。每个世界实例可以有不同的规则集。

### 8.1 核心理念

Screeps 的问题是**规则硬编码**——出生点逻辑、代码更新成本、drone 控制权限都是引擎的一部分，社区服主无法修改。Swarm 把这些做成**世界级配置 + ECS Plugin**。

```
世界配置 (WorldConfig)          ECS Plugin (System 注入)
┌─────────────────────┐        ┌──────────────────────┐
│ spawn_policy         │        │ SpawnPolicySystem    │
│ code_update_cost     │   →    │ CodeUpdateCostSystem │
│ code_propagation     │        │ PropagationSystem    │
│ drone_env_vars       │        │ DroneEnvVarSystem    │
│ ...                  │        │ ...                  │
└─────────────────────┘        └──────────────────────┘
         │                              │
         └──────────┬───────────────────┘
                    ▼
            引擎启动时加载
```

### 8.2 规则分类

#### 出生与加入

| 规则 | 类型 | 说明 |
|------|------|------|
| `spawn_policy` | enum | `RandomRoom`（默认）\\| `ManualSelect`（玩家选坐标，仅在首次加入/重生时）\\| `FixedSpawn`（固定出生点）\\| `Inherit`（从已有殖民地出生——需该房间存在玩家的 Controller 且 level ≥ 1） |
| `spawn_cooldown` | u32 | 新玩家加入后多少 tick 才能开始操作（默认 0） |
| `respawn_policy` | enum | 殖民地全灭后的处理：`NewRoom` \| `SameRoom` \| `Spectate` \| `Ban` |

#### 代码部署

| 规则 | 类型 | 说明 |
|------|------|------|
| `code_update_cost` | ResourceCost | 部署新 WASM 消耗的资源（默认 `{Energy: 0}` — 免费） |
| `code_update_cooldown` | u32 | 两次部署间的最小 tick 间隔（默认 5，World 模式最小 5，防止 re-deploy refund 滥用） |
| `code_update_window` | (u32, u32) | 部署窗口期：每 N tick 开放 M tick（默认无限制） |
| `code_propagation_speed` | u32 | 代码更新传播速度：0=全局即时，>0=每 tick 传播 N 格 |
| `code_propagation_source` | enum | 传播源：`Spawn`（从出生点传播）\| `Controller`（从控制器传播）\| `AnyDrone` |

#### Drone 控制

| 规则 | 类型 | 说明 |
|------|------|------|
| `env_vars` | bool | 是否允许给 drone 设置环境变量（`drone.set("role", "harvester")`） |
| `memory_size` | u32 | 每 drone 最大环境变量存储（bytes，默认 1024） |
| `memory_spawn_cost` | `{String: u32}` | 每 byte 内存的孵化成本 × 精度因子（默认 `{}` = 免费） |
| `memory_upkeep_cost` | `{String: u32}` | 每 byte 内存的每 tick 维护费 × 精度因子（默认 `{}` = 免费） |

**手动控制不开放**：manual_control 与「代码就是军队」的核心哲学冲突，已删除。唯一例外是 Tutorial 专用世界中的受限引导操作——但 Tutorial 世界独立运行，不与正式世界互通。

#### 资源与经济

| 规则 | 类型 | 说明 |
|------|------|------|
| `source_regeneration_rate` | `fixed<u32,4>` | 资源点再生速率倍率 × 10000（默认 10000 = 1.0） |
| `build_cost_multiplier` | `fixed<u32,4>` | 建筑成本倍率 × 10000（默认 10000 = 1.0） |
| `drone_decay_rate` | `fixed<u32,4>` | drone 衰减倍率 × 10000（默认 10000 = 1.0） |

#### Drone 生命周期

| 规则 | 类型 | 默认 | 说明 |
|------|------|------|------|
| `drone_lifespan` | u32 | 1500 | drone 最大存活 tick 数。达到后自动死亡（`death_cleanup_system` 回收）。**续期**: 玩家拥有的每个 Controller 每 tick 给全局所有 drone 回退 age 0.5 tick（多 Controller 可叠加，上限为完全抵消自然 age 增长）。不再依赖一次性占领事件。**冷却**: 无——改为持续维持模型，消除"占领-放弃-再占领"的 farming 策略 |

#### Drone 身体规划

**body 不可逆**: 一旦 spawn，body part 组成不可更改。但可通过 `Recycle` 回收 drone 获得 50% 资源退还，重新 spawn 更优 body。

**新手保护**: Tutorial 世界前 500 tick 回收退还 100%（新人可以试错）。标准世界回收退还 50%。

#### 自定义资源类型

世界可以定义任意种类和数量的资源。默认世界只有 `Energy` 一种资源——但服主可以定义 `Crystal + Gas`（星际争霸风格）、`Food + Wood + Stone + Gold`（帝国时代风格）、或 `CPU + Memory + Bandwidth`（赛博朋克主题）。

| 规则 | 类型 | 说明 |
|------|------|------|
| `resource_types` | `[ResourceDef]` | 世界中的资源类型列表，默认 `[{name: "Energy"}]` |

#### 资源存储模型：全局 vs 本地

玩家的资源分为两层：

```
全局存储 (Player Storage)          本地存储 (World Storage)
┌─────────────────────┐           ┌──────────────────────┐
│ 抽象经济力量          │           │ 物理存在于建筑中        │
│ 不依赖建筑            │  物流成本  │ 需要 Storage/Extension │
│ 可市场交易            │ ←──────→ │ drone 采集先到这里     │
│ 可支付部署费          │           │ 跨房间运输需要 Carry    │
│ 有容量上限（研究升级）  │           │ 可被敌方掠夺/摧毁      │
└─────────────────────┘           └──────────────────────┘
```

**默认行为**：
- drone 采集资源 → 先进入**世界本地存储**（就近的 Storage/Extension/Spawn）
- 世界本地存储的资源可通过 Terminal 在市场交易（需物流可达）
- 玩家可将本地存储转为全局存储（消耗能量 + 时间 = 物流成本）
- 全局存储的资源在部署代码、支付维护费时自动扣除
- 全局存储不能直接用于本地建造——需先转回本地

**可配置参数**：

| 规则 | 类型 | 默认值 | 说明 |
|------|------|--------|------|
| `global_storage_enabled` | bool | true | 是否启用全局存储。false = 纯本地物流 |
| `global_storage_capacity` | u32 | 100000 | 全局存储上限 |
| `transfer_to_global_cost` | ResourceCost | `{Energy: 0.01}` | 本地→全局每单位资源的转换成本（默认 1%） |
| `transfer_to_global_time` | u32 | 10 | 转换所需的 tick 数（不可为 0，防止瞬移补给） |
| `transfer_from_global_cost` | ResourceCost | `{Energy: 0.05}` | 全局→本地每单位资源的转换成本（默认 5%） |
| `transfer_from_global_time` | u32 | 5 | 全局→本地转换所需 tick 数（不可为 0） |

**三种物流模式**：

```
模式 A: 无物流 (global_storage_enabled=true, transfer_cost=0)
  drone采集 → 即时进入全局存储 → 任何地方可用
  最简单，适合新手和快节奏 Arena

模式 B: 轻物流 (默认)
  drone采集 → 本地存储 → 付1%转全局 → 全局付部署费
  全局→本地付5% → 本地建造
  有策略深度但不过度惩罚

模式 C: 硬核物流 (global_storage_enabled=false)
  所有资源物理存在，必须用 Carry drone 运输
  类似 Factorio——物流本身就是核心玩法
```

**市场交易的物流规则**：

- 全局存储中的资源 → 可即时挂单交易（无物流延迟）
- 本地存储中的资源 → 需要先转全局，或通过 Terminal 建筑交易
- 买入的资源 → 进入全局存储（需转本地才能用于建造）
- 世界规则可配置：`market_requires_terminal = true/false`

#### 全局存储反制机制（Anti-Dominant-Strategy）

为防止富有玩家通过囤积全局存储垄断经济、操纵市场价格、阻断新玩家供给，设计以下三项内置反制：

**1. 累进存储税（Progressive Storage Tax）**

玩家全局存储总量超过阈值后，超出部分按累进税率征收每 tick 维护费：

| 存储量（占容量上限） | 税率（每 tick） |
|---|---|
| 0–30% | 0%（免税） |
| 30–60% | 0.01%（每万单位 1 单位） |
| 60–85% | 0.05% |
| 85–100% | 0.20% |

> 税率由世界规则配置 `global_storage_tax_tiers` 控制。Arena 模式默认免税（竞技公平）。

**2. 本地存储隐匿性（Stealth Advantage）**

- **全局存储余额**：部分公开——排行榜可显示排名区间，市场挂单暴露部分余额
- **本地存储**：完全私有——敌方无法获知你的建筑中存了多少资源，直到发起侦察或占领

这使得囤积本地存储成为战略优势：敌方不知道你的真实经济实力。

**3. 全局↔本地转换需物流运输（No Teleport）**

- `transfer_to_global_time`：本地→全局转换需 N tick（默认 10 tick）。资源在运输期间不可用。
- `transfer_from_global_time`：全局→本地转换需 N tick（默认 5 tick）。大型帝国需提前规划补给线。
- 转换期间资源处于"运输中"状态——可被敌方巡逻 drone 拦截（需 PvP 启用，Phase 6 战斗系统实现）。

> 运输时间使全局存储不能作为"战斗中的即时补给"——这是一种非平凡的策略权衡。

| 规则 | 类型 | 默认值 | 说明 |
|------|------|--------|------|
| `global_storage_tax_tiers` | `[(u32, u32)]` | `[(30,0),(60,1),(85,5),(100,20)]` | 累进税率：(容量%, 每10万单位税率) |
| `transfer_to_global_time` | u32 | 10 | 本地→全局转换所需 tick 数（不可为 0） |
| `transfer_from_global_time` | u32 | 5 | 全局→本地转换所需 tick 数（不可为 0） |
| `global_storage_public` | bool | false | 全局存储是否完全公开（默认仅排行榜区间） |

#### 资源定义

```toml
[[resource_types]]
name = "Crystal"              # 资源名（标识符）
display_name = "水晶矿"        # 显示名
category = "mineral"          # mineral | gas | organic | energy
starting_amount = 0           # 新玩家初始拥有量
max_storage = 100000          # 单玩家最大储量
decay_rate = 10               # 每 tick 衰减比例 × 10000（0 = 不衰减）
tradeable = true              # 是否可在市场交易
```

定义了资源类型后，可以给不同的动作指定不同的资源消耗：

```toml
[actions.costs]

# Spawn drone 消耗：水晶 + 高能瓦斯
spawn = { Crystal = 200, Gas = 50 }

# 建造建筑
build.Extension = { Crystal = 50 }
build.Tower = { Crystal = 100, Gas = 25 }

# 生成 body part
body_part.Move = { Crystal = 50 }
body_part.Work = { Crystal = 100 }
body_part.Attack = { Crystal = 80, Gas = 20 }
body_part.Heal = { Crystal = 250, Gas = 100 }
body_part.Claim = { Crystal = 600 }

# 代码部署
code_update = { Crystal = 500 }

# 维修
repair_per_hit = { Crystal = 1 }
```

资源点可以产出多种资源：

```toml
[[source_types]]
name = "CrystalField"
produces = { Crystal = 1 }     # 每 tick 产出
capacity = 3000
regeneration = 300             # 每 tick 再生量

[[source_types]]
name = "GasVent"
produces = { Gas = 1 }
capacity = 2000
regeneration = 10
```

#### 战斗与 PvP

| 规则 | 类型 | 说明 |
|------|------|------|
| `pvp_enabled` | bool | 是否允许 PvP（默认 true） |
| `friendly_fire` | bool | 是否允许攻击同阵营（默认 false） |
| `damage_multiplier` | `fixed<u32,4>` | 伤害倍率 × 10000（默认 10000 = 1.0） |

#### 伤害与武器类型

伤害类型和抗性体系是**世界规则的一部分**——像资源类型一样可由 world.toml 定义和模组扩展。默认世界提供以下基础类型：

```toml
# world.toml — 伤害类型定义（可扩展）
[[damage_types]]
name = "Kinetic"
description = "动能冲击——碰撞、钝击、爆炸"
default_resistance = 1.0

[[damage_types]]
name = "Thermal"
description = "热能——火焰、激光、等离子"
default_resistance = 1.0

[[damage_types]]  
name = "EMP"
description = "电磁脉冲——电击、过载、电子干扰"
default_resistance = 1.0

[[damage_types]]
name = "Sonic"
description = "声波——振动、共振、超声波"
default_resistance = 1.0

[[damage_types]]
name = "Corrosive"
description = "腐蚀——酸液、纳米分解、生化"
default_resistance = 1.0

[[damage_types]]
name = "Psionic"
description = "心灵——精神攻击、认知干扰、AI 劫持"
default_resistance = 1.0

# 抗性：按 body part / structure / 属性叠加
# 抗性倍率相乘: final_multiplier = body_resistance × attribute_resistance
[resistances.Tough]
Kinetic = 0.5          # 肉盾：动能减半
Sonic = 0.5            # 减震

[resistances.Structure]
EMP = 2.0              # 建筑弱电磁
Corrosive = 1.5        # 建筑怕腐蚀

# 属性级抗性（Rhai 模组可为实体动态赋予）
# 例如: actions.set_attribute(entity_id, "Shielded", true)
#       → 所有伤害 × 0.7 (需在 world.toml 定义 attribute_multipliers)
```

**Body part 伤害绑定**:

| Body Part | 默认伤害类型 | 基础伤害值 | 成本 | 说明 |
|-----------|------------|----------|------|------|
| Attack | Kinetic | 30 | 80E | 近战（距离 1），低成本高伤害 |
| RangedAttack | Kinetic | 25 | 100E | 远程（距离 3），射程优势 |
| Tower（建筑自动攻击） | Kinetic | 50 | — |
| Heal | —（反向治疗） | 12 | 250E | 每 tick 可缩短一个负面状态 10 tick 持续时间 |

**抗性机制**: 分两层叠加——**组件抗性**（body part / structure 的固定倍率）+ **属性抗性**（由模组/规则动态赋予的倍率，如 `Shielded = 0.7`）。最终倍率 = 组件倍率 × 属性倍率。

**免疫机制**: Rhai 模组可通过 `actions.set_entity_flag(entity_id, "immune_Thermal", true)` 赋予免疫（倍率 = 0）。适用于 Boss 单位、世界事件、特殊建筑。

**模组扩展**: Rhai 模组可注册新伤害类型（`actions.add_damage_type("Fire", 1.0)`）、设置抗性（`actions.set_resistance("Tough", "Fire", 0.3)`）、赋予属性（`actions.set_attribute(entity_id, "Flaming", true)`）。

#### 特殊攻击方式

除了 HP 伤害，以下特殊攻击方式作为 Command 或 body part 能力存在：

| 攻击方式 | 触发 body part | 效果 | 冷却 | 资源消耗 | 抗性 |
|---------|--------------|------|------|---------|------|
| **Hack** | Claim | 夺取目标 drone：施加"控制锁"逐步建立控制——tick 1-2 目标减速 50%，tick 3-4 目标无法移动，tick 5 夺取成功（drone 转为 Neutral，停止执行 WASM，进入 idle）。5 tick 后自动恢复。idle 期间不消耗 lifespan。目标可通过 Disrupt 打断或 Fortify 净化控制锁 | 200 tick | 1000 Energy | 目标 `Psionic` 抗性 |
| **Drain** | Carry + Work | 从目标建筑/存储中窃取资源，每 tick 转移 `carry_capacity` 单位 | 50 tick | 200 Energy/tick | 目标 `EMP` 抗性 |
| **Overload** | RangedAttack | 消耗目标计算配额。目标 `fuel budget` 减少 500k（默认 MAX_FUEL=10M 的 5%）。**下限 MAX_FUEL × 0.2** | 200 tick | 300 Energy | 目标 `EMP` 抗性 |
| **Debilitate** | Work | 给目标附加易伤状态。指定伤害类型抗性 ×2，持续 50 tick | 150 tick | 200 Energy | 目标 `Corrosive` 抗性 |
| **Disrupt** | Attack | 打断目标当前动作（Drain/Hack 等持续动作立即终止）。不造成 HP 伤害 | 50 tick | 100 Energy | 目标 `Sonic` 抗性 |
| **Fortify** | Tough | 自身/友方获得护盾（所有抗性 ×0.5）。**同时清除目标所有负面状态**（Debilitate/Drain/Overload/Hack控制锁），持续 100 tick | 300 tick | 400 Energy | 无——增益+净化 |

**通用规则**：
- 特殊攻击与 HP 伤害互斥——同一 body part 在同一 tick 只能执行一种
- 特殊攻击的"命中判定"取决于 body part 数量与目标防御的差值，非简单的命中/未命中
- 持续型攻击（Drain/Hack）在 drone 移动或被 Disrupt 时中断
- 所有特殊攻击受 `damage_multiplier` 世界规则影响（倍率作用于成功率/效果量）

**Neutral 状态**（Hack 夺取后）:
- `owner = Neutral (0)`——不归任何玩家所有
- 停止执行 WASM（进入 idle 状态，不提交指令）
- 不消耗 lifespan、不消耗 fuel
- 5 tick 后自动恢复原 owner（Hack 自然到期）
- 恢复前免疫再次 Hack
- 可见性：对原 owner 保持可见（ally 级），对其他玩家为 enemy 级

**Body part 扩展**：世界可通过 `[[body_part_types]]` 定义新 body part 并绑定伤害类型或特殊攻击。模组可引入 `Leech`（吸血）、`Scramble`（随机改变目标代码执行顺序）、`Fabricate`（将敌方 drone 转化为己方建筑）等。

#### 可见性与观战

可见性分两层：**drone 感知**（影响游戏公平性）和**玩家视野**（影响观战体验）。

##### Drone 感知（进入 snapshot）

| 规则 | 类型 | 默认 | 说明 |
|------|------|------|------|
| `fog_of_war` | bool | true | drone 的 WASM `tick()` snapshot 是否受可见性限制。true = drone 只能"看到"感知范围内的实体（视觉/听觉/嗅觉分层）；false = snapshot 包含全地图（合作/教学世界） |

##### 玩家视野（人类屏幕 / AI MCP 查看）

| 规则 | 类型 | 默认 | 说明 |
|------|------|------|------|
| `player_view` | enum | `"drone"` | `"drone"` = 玩家只能看到自己 drone 所见；`"full"` = 玩家实时看到全地图（无论 drone 感知范围）；`"allied"` = 看到所有同阵营 drone 的聚合视野 |
| `public_spectate` | bool | false | 是否允许未登录用户实时旁观（只读 WebSocket）。World 默认关，Arena 默认开 |
| `spectate_delay` | u32 | 0 | 旁观延迟（tick 数）。0 = 实时；>0 = 延迟回放，防止观众信息泄露给参赛者 |
| `replay_privacy` | enum | `"private"` | 回放可见性：`"private"` = 仅自身；`"allies"` = 同阵营可看；`"world"` = 同世界玩家可看；`"public"` = 任何人（含未登录）。Arena 模式赛后强制 `"public"` |

**组合示例**：

| 场景 | fog_of_war | player_view | 效果 |
|------|-----------|-------------|------|
| 标准 World | true | drone | drone 感知有限，玩家只看自己 drone 所见 |
| 教学世界 | false | full | 新手看到全地图，drone 也能感知全图 |
| 竞技观战 | true | drone | drone 公平受限，但观众通过 `public_spectate` + `spectate_delay=100` 看延迟全图 |
| 合作 PvE | true | allied | drone 各自感知，但玩家看到所有友方聚合视野 |

### 8.3 配置格式

```toml
# world.toml — 每个世界实例的配置文件

[world]
name = "World of Swarm"
mode = "persistent"              # persistent | arena

[spawn]
policy = "RandomRoom"
respawn = "NewRoom"
cooldown = 100                   # 加入后 100 tick 才能操作

[code]
update_cost = { Energy = 500 }   # 部署消耗 500 能量
update_cooldown = 100            # 两次部署间隔 100 tick
update_window = { every = 1000, duration = 100 }  # 每 1000 tick 开放 100 tick 窗口
propagation_speed = 3            # 每 tick 传播 3 格
propagation_source = "Spawn"     # 从出生点向外传播

[drone]
env_vars = true                  # 允许环境变量
memory_size = 2048               # 每 drone 2KB 存储
lifespan = 1500                  # drone 存活 tick 数上限
memory_spawn_cost = { Energy = 0.5 }     # 每 byte 孵化成本
memory_upkeep_cost = { Energy = 0.01 }   # 每 byte 每 tick 维护费

[visibility]
fog_of_war = true                # drone 感知受可见性限制
player_view = "drone"            # 玩家只看自己 drone 所见
public_spectate = false          # World 模式默认不公开旁观
spectate_delay = 0               # 回放无延迟

[resources]
source_regeneration = 1.0
build_cost = 1.0
drone_decay = 1.0

# 物流配置
global_storage_enabled = true
global_storage_capacity = 100000
transfer_to_global_cost = { Energy = 0.01 }    # 1% 损耗
transfer_from_global_cost = { Energy = 0.05 }   # 5% 损耗
market_requires_terminal = true

# 自定义资源类型
[[resource_types]]
name = "Energy"
display_name = "能量"
category = "energy"
starting_amount = 1000
max_storage = 100000

[[resource_types]]
name = "Matter"
display_name = "物质"
category = "mineral"
starting_amount = 500
max_storage = 50000

# 各动作资源消耗
[actions.costs]
spawn = { Energy = 200, Matter = 50 }
build.Extension = { Energy = 50 }
build.Tower = { Energy = 100, Matter = 25 }
body_part.Move = { Energy = 50 }
body_part.Work = { Energy = 100 }
body_part.Attack = { Energy = 80, Matter = 20 }
body_part.Heal = { Energy = 250, Matter = 100 }
code_update = { Energy = 500 }
repair_per_hit = { Energy = 1 }

# 资源点类型
[[source_types]]
name = "EnergyField"
produces = { Energy = 1 }
capacity = 3000
regeneration = 300

[[source_types]]
name = "MatterDeposit"
produces = { Matter = 1 }
capacity = 2000
regeneration = 10

[combat]
pvp = true
friendly_fire = false
damage = 1.0
```

### 8.4 ECS 集成方式

每个规则类别对应一个可选的 ECS System。引擎启动时读取 `world.toml`，有选择地注册 System：

```rust
// engine 启动时
fn register_rule_systems(app: &mut App, config: &WorldConfig) {
    // 基础系统始终注册（Phase 2b 系统链——Inline 命令已在 Phase 2a 逐条执行）
    app.add_systems(Update, (
        death_mark_system,       // 标记待死亡 entity，释放 room cap
        spawn_system,            // 统一创建校验通过的 drone
        regeneration_system,     // 资源点再生
        combat_system,           // 战斗结算（damage 先 → heal 后）
        decay_system,            // 疲劳/冷却递减
        death_cleanup_system,    // 实际 despawn
    ).chain());

    // 注入资源注册表——所有 System 通过它查询资源类型和消耗
    let resource_registry = ResourceRegistry::from_config(&config);
    app.insert_resource(resource_registry);

    // 规则系统按配置注册
    if config.code.propagation_speed > 0 {
        app.add_systems(Update, code_propagation_system.before(spawn_system));
    }
    if config.drone.memory_upkeep_cost.len() > 0 {
        app.add_systems(Update, memory_upkeep_system.before(decay_system));
    }
    // ...
}

// ResourceRegistry 是运行时的资源类型字典
struct ResourceRegistry {
    types: IndexMap<String, ResourceDef>,  // IndexMap 保证迭代顺序确定
    action_costs: ActionCosts,       // spawn, build.*, body_part.*, ...
    source_types: Vec<SourceDef>,
}

impl ResourceRegistry {
    /// 查询某个动作的资源消耗
    fn cost(&self, action: &str, detail: Option<&str>) -> HashMap<String, u32> {
        // action = "build", detail = "Tower"
        // → { "Energy": 100, "Matter": 25 }
    }
}
```

关键是：**核心引擎不硬编码 Energy**。它只操作 `HashMap<ResourceName, Amount>`。资源名是配置决定的字符串。

```rust
// 之前（硬编码）
struct Resource { energy: u32 }

// 之后（动态）
struct Resource {
    amounts: IndexMap<String, u32>,  // IndexMap 保证迭代顺序确定
}
struct ResourceDef {
    name: String,
    display_name: String,
    category: ResourceCategory,
    starting_amount: u32,
    max_storage: u32,
    decay_rate: u32,  // 每 tick 衰减比例 × 精度因子（0 = 不衰减）
    tradeable: bool,
}
```

### 8.5 WASM 侧感知（Deferred 模型）

WASM 模块通过 `tick(snapshot_json) → commands_json` 延迟模型运作：
- 引擎将快照 JSON 写入 WASM 线性内存，调用 `tick()`
- WASM 模块通过**查询 host function**（get_terrain、get_objects_in_range、path_find、get_world_config）读取世界状态
- `tick()` 返回指令 JSON 列表，引擎在校验后统一应用

```typescript
// TypeScript SDK — tick() 接收 Snapshot，返回 Command[]
function tick(snapshot: WorldSnapshot): Command[] {
    // 查询世界配置（只读 host function）
    const registry = snapshot.resourceRegistry;

    // 查看世界中定义了哪些资源
    for (const [name, def] of registry.types) {
        console.log(`${name} (${def.display_name}): max ${def.maxStorage}`);
    }

    // 查询动作消耗
    const spawnCost = registry.cost("spawn");
    // → { Energy: 200, Matter: 50 }

    // 生成指令列表
    const commands: Command[] = [];

    // 检查资源 → 决定指令
    if (snapshot.player.resources.has(spawnCost)) {
        commands.push({ cmd: "spawn", body: [...] });
    }

    // 采集指令
    commands.push({ cmd: "harvest", target: sourceId, resource: "Matter" });

    // 传输指令
    commands.push({ cmd: "transfer", target: targetId, resources: { Energy: 100, Matter: 50 } });

    // 返回指令 JSON — 引擎统一校验后执行
    return commands;
}
```

> **设计合同**: WASM 模块通过 `tick() → JSON` 延迟模型运作。所有 mutating 操作以 JSON 指令形式返回，引擎统一校验和应用。不得通过 host function 直接修改世界状态。

### 8.6 World 与 Arena 的默认规则

| 规则 | World 默认值 | Arena 默认值 |
|------|------------|------------|
| `spawn_policy` | `RandomRoom` | `FixedSpawn`（对称） |
| `code_update_cost` | 0（免费） | 0 |
| `code_update_window` | 无限制 | 赛前锁定 |
| `code_propagation_speed` | 0（即时） | 0（即时） |
| `drone_env_vars` | true | true |
| `pvp_enabled` | true | true（必须） |

### 8.7 Rule Module System — 可安装的游戏模组

规则模组是**可安装的 Rhai 脚本 + 声明式配置**——轻量、确定、可组合。

```
玩家代码:  WASM → 控制 drone     (不可信 → sandbox)
规则模组:  Rhai → 修改世界规则    (服主声明 → 引擎嵌入)
引擎核心:  Rust → 确定性模拟      (不可变)
```

#### 为什么不是 WASM

| | WASM（玩家） | Rhai（规则） |
|------|-------------|------------|
| 信任模型 | 不可信，需要进程隔离 | 服主自行安装，可信 |
| 编译步骤 | 需要外部工具链 | 无，引擎直接执行源码 |
| 确定性 | 依赖 wasmtime 版本 | 同引擎版本完全确定 |
| 语言复杂度 | 取决于源语言 | 极简，类似 Rust/JS |
| 性能 | JIT | AST 解释（规则场景足够） |

#### 模组结构

一个模组是一个目录：

```
empire-upkeep/
├── mod.toml          # 模组元数据 + 可配置参数声明
├── init.rhai         # 加载时执行一次
├── tick_start.rhai   # 每 tick 开始时执行
└── tick_end.rhai     # 每 tick 结束时执行
```

##### mod.toml

```toml
[meta]
name = "empire-upkeep"
version = "1.2.0"
description = "帝国规模维护费——drone 和房间越多，每 tick 消耗越大"
author = "kagurazaka"
license = "MIT"
dependencies = []       # 依赖的其他模组
conflicts = []          # 冲突的模组

# 可配置参数——每项在脚本中作为全局变量可用
[config]
drone_cost = { type = "u32", default = 2, min = 0, max = 100, description = "每 drone 每 tick 维护费" }
room_base = { type = "u32", default = 10, min = 0, max = 1000, description = "每房间基础维护费" }
room_superlinear = { type = "fixed<u32,4>", default = 1, min = 0, max = 100, description = "超线性系数（定点数，4位小数精度）" }
onshortfall = { type = "enum", default = "degrade", values = ["degrade", "damage", "despawn"], description = "资源不足时的处理方式" }
```

##### init.rhai

```rust
// 模组加载时执行一次——验证配置、初始化内部状态
fn init(config, actions) {
    actions.log_info(`empire-upkeep v${MOD_VERSION} loaded`);
    actions.log_info(`  drone_cost=${config.drone_cost}`);
    actions.log_info(`  room_superlinear=${config.room_superlinear}`);
    actions.log_info(`  onshortfall=${config.onshortfall}`);
}
```

##### tick_end.rhai

```rust
// 每 tick 结束时执行——计算维护费并扣除
fn on_tick_end(state, events, config, actions) {
    for player in state.players() {
        let drones = player.drones().len();
        let rooms = player.rooms().len();

        // 超线性：房间越多，每房间成本越高
        // room_superlinear 为 fixed<u32,4> 定点数（4位小数精度）
        let room_penalty = rooms * (config.room_base +
            rooms * config.room_superlinear / FIXED_SCALE);

        let total_cost = drones * config.drone_cost + room_penalty;

        actions.deduct_resource(player.id, "Energy", total_cost);
        actions.emit_event("upkeep_charged", #{
            player: player.id,
            drones: drones,
            rooms: rooms,
            cost: total_cost
        });
    }
}
```

#### Rhai API：模组可用的函数

```rust
// 状态查询（经可见性过滤——模组不能看到隐藏实体）
state.players()          → Iterator<Player>        // 聚合统计，不暴露具体玩家
state.tick()             → u64
player.drones()          → Iterator<Drone>          // 仅该玩家的 drone（owner=player_id）
player.rooms()           → Iterator<Room>           // 仅该玩家有视野的房间
player.resources()       → Map<String, u64>         // 仅该玩家的资源
drone.body_parts()       → Vec<BodyPart>
drone.position()         → (x, y, room_id)

// 世界修改（通过 actions，不进命令管线但经 mini-validator）
actions.deduct_resource(player_id, resource, amount)   // 扣除资源
actions.award_resource(player_id, resource, amount)    // 奖励资源
actions.damage_entity(entity_id, amount, reason)       // 对实体造成伤害
actions.set_entity_flag(entity_id, flag, value)        // 设置白名单标记（如 slow/empowered）
actions.emit_event(event_type, data)                   // 发出事件
actions.log_info(message)                              // 日志
actions.log_warn(message)

// 不可用: modify_entity（无属性白名单，已删除）
// 不可用: 文件 IO、网络、时钟、随机数（确定性要求）
```

#### Rhai 执行预算

每个模组每次 `tick_start` / `tick_end` 钩子的执行预算：

| 资源 | 限制 | 超限行为 |
|------|------|---------|
| AST 节点数 | 10,000/tick | 该模组本次 tick 跳过，记录警告 |
| actions 调用次数 | 100/tick | 超出部分丢弃 |
| `state.players()` 迭代 | 3,000 项 | 超出的玩家跳过 |
| 墙钟执行时间 | 100ms/tick | 强制终止当前模组，**该模组本 tick 的所有 actions 全部回滚**（事务性隔离）。仅防拖垮引擎——正常模组不应触发，触发视为 bug。回滚确保 state_checksum 不受部分执行影响 |

> 连续 10 tick 超限的模组自动禁用，需服主手动重新启用。防止恶意/错误模组拖垮引擎。

所有 `actions` 操作被记录到 TickTrace——可回放、可审计。

#### 安装与配置

```bash
# 从模组市场安装
swarm mod install empire-upkeep

# 查看模组的可配置项
swarm mod config empire-upkeep

# 设置参数
swarm mod config empire-upkeep drone_cost 5
swarm mod config empire-upkeep onshortfall "damage"

# 在世界中启用
swarm world add-mod empire-upkeep
```

世界配置中引用：

```toml
# world.toml
[world]
name = "Survival World"

[[mods]]
name = "empire-upkeep"
version = "1.2.0"
[mods.config]
drone_cost = 5
room_superlinear = 2        # fixed<u32,4>: 0.0002 超线性系数
onshortfall = "damage"

[[mods]]
name = "resource-decay"
version = "0.3.0"
[mods.config]
decay_rate = 0.001
```

#### 引擎集成

```rust
fn register_mod_systems(app: &mut App, world_config: &WorldConfig) {
    for mod_def in &world_config.mods {
        let mut module = load_mod(&mod_def.name, &mod_def.version);
        module.configure(&mod_def.config);                // 注入参数
        module.run_init();                                 // init.rhai

        // 注册 tick 钩子
        let tick_end = module.tick_end_script.clone();
        app.add_systems(Update, move |world: &mut World| {
            let state = WorldState::from_world(world);
            let mut actions = RuleActions::new();
            let events = TickEvents::current();
            tick_end.call(&state, &events, &module.config, &mut actions);
            actions.apply(world);  // 经校验后写入
        }.after(death_cleanup_system));
    }
}
```

#### 模组市场

```
swarm-mods.kagurazakalan.com

  模组              评分    安装量    描述
  ─────────────────────────────────────────────────
  empire-upkeep     ★4.8   1,234     帝国规模维护费
  fog-of-war        ★4.6   892       战争迷雾
  resource-decay    ★4.3   567       资源腐败衰减
  territory-control ★4.5   445       连续领土要求
  alliance-system   ★4.7   678       玩家间结盟
  mutation          ★4.2   234       drone 进化变异
```

模组是源码——服主可以 fork、修改、提交 PR。社区 review + rating。

#### 规则可见性与 i18n

世界的活跃规则对所有玩家（人类和 AI）完全可见。每个配置项都有多语言描述。

##### mod.toml 中的 i18n

```toml
[meta]
name = "empire-upkeep"
version = "1.2.0"
description = "帝国规模维护费"

# 多语言描述
[meta.description_i18n]
zh = "帝国规模维护费——drone 和房间越多，每 tick 消耗越大。维护费不足时效率下降。"
en = "Empire upkeep — more drones and rooms cost more per tick. Shortfall degrades efficiency."
ja = "帝国維持費——ドローンと部屋が多いほど毎 tick のコストが増加。不足時は効率低下。"

[config]

[config.drone_cost]
type = "u32"
default = 2
min = 0
max = 100
[config.drone_cost.description_i18n]
zh = "每架 drone 每 tick 消耗的能量"
en = "Energy consumed per drone per tick"
ja = "ドローン1機あたりの毎 tick エネルギー消費"

[config.room_superlinear]
type = "fixed<u32,4>"
default = 1
min = 0
max = 100
[config.room_superlinear.description_i18n]
zh = "超线性系数——房间越多，每间房的单位成本越高"
en = "Superlinear factor — more rooms increase per-room cost"
ja = "超線形係数——部屋が増えるほど1部屋あたりのコストが上昇"

[config.onshortfall]
type = "enum"
default = "degrade"
values = ["degrade", "damage", "despawn"]
[config.onshortfall.description_i18n]
zh = "资源不足时的处理方式：degrade=效率下降, damage=建筑受损, despawn=单位消亡"
en = "Behavior on resource shortfall: degrade=slow, damage=hurt buildings, despawn=lose units"
ja = "リソース不足時の動作：degrade=効率低下, damage=建物損傷, despawn=ユニット消滅"
[config.onshortfall.values_i18n]
degrade = { zh = "效率下降", en = "Efficiency degradation", ja = "効率低下" }
damage = { zh = "建筑受损", en = "Building damage", ja = "建物損傷" }
despawn = { zh = "单位消亡", en = "Unit despawn", ja = "ユニット消滅" }
```

##### 玩家可见的世界规则

人类玩家在 Web UI 中看到：

```
┌─────────────────────────────────────────────┐
│  世界规则 — Survival World                    │
│                                               │
│  🔧 empire-upkeep v1.2.0                      │
│  帝国规模维护费——drone 和房间越多，每 tick    │
│  消耗越大。维护费不足时效率下降。               │
│                                               │
│  当前参数:                                     │
│    drone_cost = 5        每架 drone 每 tick   │
│                          消耗的能量             │
│    room_superlinear = 0.2                     │
│                          超线性系数             │
│    onshortfall = damage  资源不足时的处理方式    │
│                                               │
│  🍂 resource-decay v0.3.0                     │
│  资源腐败衰减——储存的资源随时间缓慢减少。        │
│                                               │
│  当前参数:                                     │
│    decay_rate = 0.001   每 tick 衰减比例        │
└─────────────────────────────────────────────┘
```

AI 玩家通过 MCP 查询：

```
mcp.call("swarm_get_world_rules")
→ {
  "mods": [
    {
      "name": "empire-upkeep",
      "version": "1.2.0",
      "description": "帝国规模维护费——drone 和房间越多...",
      "config": {
        "drone_cost": { "value": 5, "type": "u32", "min": 0, "max": 100,
                        "description": "每架 drone 每 tick 消耗的能量" },
        "room_superlinear": { "value": 2, "type": "fixed<u32,4>",
                              "description": "超线性系数——房间越多..." },
        "onshortfall": { "value": "damage", "type": "enum",
                         "values": ["degrade","damage","despawn"],
                         "description": "资源不足时的处理方式" }
      }
    }
  ]
}
```

##### WASM 侧查询

玩家的 drone 代码可以查询当前世界规则：

```typescript
// TypeScript SDK
const rules = Game.world.rules();

for (const mod of rules.active_mods) {
    console.log(`${mod.name} v${mod.version}`);
    console.log(`  ${mod.description}`);
    for (const [key, param] of mod.config) {
        console.log(`  ${key} = ${param.value}  // ${param.description}`);
    }
}

// 根据规则调整策略
if (rules.get("empire-upkeep").config.onshortfall.value === "damage") {
    // 维护费不足会损坏建筑——必须保持能量正流入
    strategy.prioritize_energy_income();
}
```

##### 语言选择

引擎根据请求的 `Accept-Language` 头或 MCP 客户端的 `locale` 参数返回对应语言的描述。缺少翻译时回退到 `en`，再回退到 `description` 字段。

#### 帝国维护费示例效果

```
小帝国（1 房, 20 drone）: 维护费 ≈ 40/tick     — 轻松
中帝国（5 房, 100 drone）: 维护费 ≈ 275/tick   — 可承受
大帝国（20 房, 500 drone）: 维护费 ≈ 2100/tick  — 需要高效经济
巨帝国（50 房, 500 drone）: 维护费 ≈ 3150/tick — 硬上限

不是不可逾越——达到上限前「你能支撑多大就有多大」。
想维持巨帝国？你的 drone 物流必须极致优化。
```

### 8.8 Determinism Contract — 确定性合同

#### 固定算法

| 组件 | 算法 | 说明 |
|------|------|------|
| PRNG | **Blake3 XOF** | 确定种子 + offset → 随机流。与哈希同原语，消除 ChaCha 依赖，纯软件 ~6 GB/s。XOF 模式：`blake3::Hasher::update_with_seek(seed, offset)` |
| 种子 | world_seed = Blake3(32随机字节) | 32 字节熵（256-bit），编码为 hex 字符串。不可从 tick_number 推导。**每 10,000 tick 自动轮换**（Blake3(旧种子, 当前tick)），防止长期观察推断种子空间 |
| Hash | **Blake3** | 固定实现。不用 std::hash / SipHash（跨版本可变）。 |
| 种子洗牌 | Blake3(tick_number \\|\\| world_seed) | 每 tick 确定但不可预测的玩家顺序。**不是手速/运气**——玩家无法通过加快操作影响排序位置。公平随机：所有玩家同等不可预测，相同种子=相同顺序，可回放验证 |
| ECS 顺序 | `.chain()` | 严格串行。未来用 `.before()/.after()` 部分并行 |
| 数值 | 整数 + 定点数 | 禁 f64（跨平台/编译器非确定）。游戏引擎数值用 `i64 × 精度因子`。**Rhai 模组脚本同样禁用浮点**——所有模组参数必须声明为 `u32`/`i64`/`fixed<u32,N>` 定点类型，Rhai 引擎侧关闭浮点运算能力。 |
| 排序 | (shuffle_order, player_id, cmd_seq) | 相同种子 + 相同指令 → 相同顺序 |
| HashMap 顺序 | `indexmap` | 不用 std::HashMap（迭代顺序非确定） |

#### 回放保证

给定 tick N-1 状态 + tick N RawCommand + world_seed + 激活模组列表 → 相同 Wasmtime pinned 版本下 `execute_deterministic == recorded_state`。每个 tick 产出 `state_checksum` 写入 TickTrace。CI 对随机采样 tick 做 full replay 验证。

---

## 9. 路线图

### Phase 0: 架构冻结（Architecture Freeze）— ✅ 完成

- [x] Game API IDL 冻结（host functions + Command + Validator + SDK ABI + MCP schema 同源）→ P0-8
- [x] Command Source Model 冻结（12 sources: WASM/MCP_Deploy/MCP_Query/Admin/Replay/TestHarness/Tutorial/Deploy/Rollback/RuleMod/Simulate/DryRun）→ P0-9
- [x] Determinism Contract 冻结（PRNG=Blake3 XOF, hash=Blake3, 禁 f64/Rhai 浮点/禁 std::hash, IndexMap, ECS .chain()）→ DESIGN §8.8
- [x] Tick Protocol 拉齐（FDB commit in EXECUTE, tick abandon behavior, NATS ack）→ P0-1
- [x] World Rules Engine capability model 收敛为 Rhai 模组 → P0-7
- [x] Deferred Command Model 统一（tick() → JSON, 禁 imperative host functions）→ P0-4 §3
- [x] Fuel Refund 安全模型（时序/上限/滥用检测）→ P0-2 §7
- [x] 全局存储反制机制（累进税/隐匿性/运输时间）→ DESIGN §8.4
- [x] P0-9 Source Gate 完整矩阵（12 sources × capability/budget/visibility）→ P0-9
- [x] Tick 输出 JSON Schema 校验 → P0-2 §1.1

**Phase 0 冻结日期**: 2026-06-14

### 阶段概览

| Phase | 名称 | 目标 | 时间 |
|-------|------|------|------|
| 0 | 架构冻结 | 设计合同闭环 | ✅ 完成 |
| 1 | 核心 MVP | 单人垂直切片 | 4-6 周 |
| 2 | MCP + 多人 | AI/人类并行 | 6-8 周 |
| 3 | 持久化 + Rhai | 数据落地 + 模组 | 6-8 周 |
| 4 | 教程 + 调试 | 新手上手 + 回放 | 4-6 周 |
| 5 | Web 客户端 | 完整产品体验 | 6-8 周 |
| 6 | 战斗 + Arena | 游戏化收官 | 8-10 周 |
| 7 | 生产化 | 公测标准 | 8-12 周 |

详细交付物、依赖、验收标准见 [实施计划](ROADMAP.md)。

---

## 10. World 模式 vs Arena 模式

| 维度 | World（持久世界） | Arena（比赛） |
|------|-----------------|-------------|
| **本质** | 有机世界，类似 Minecraft 服务器 | 竞技比赛，类似围棋对局 |
| **地图** | 随机生成，不同玩家不同起点 | 对称初始条件，双方公平 |
| **加入时机** | 随时，先来后到不同 | 同时开始，代码在比赛前锁定 |
| **公平性** | 不追求——天然不对称 | 核心追求——对称起点 + 相同规则 |
| **运行方式** | 7×24 tick 循环 | 固定时长（例：5000 tick ≈ 4h） |
| **代码** | 随时更新（热重载） | 比赛开始时锁定 |
| **排行榜** | 无意义——起点不同无法比较 | 有意义——赛季排名、锦标赛 |
| **回放** | 自身可见，隐私分级控制 | 赛后自动公开（`replay_privacy = "public"`） |
| **旁观** | `public_spectate` 控制，默认关闭 | 默认公开（`public_spectate=true`） |
| **玩家** | 人类和 AI agent 在同一世界共存 | 1v1 或团队对决 |
| **关注点** | 持久性、创造力、涌现玩法 | 策略深度、公平性、观赏性 |

---

## 11. 贡献指南

### 11.1 开发环境搭建

```bash
git clone git@git.kagurazakalan.com:swarm/engine.git
cd engine && docker-compose up
```

### 10.2 代码规范

- Rust: `cargo fmt` + `cargo clippy`（严格）
- Go: `gofmt` + `golangci-lint`
- TypeScript: `prettier` + `eslint`（严格）
- Commit: [Conventional Commits](https://www.conventionalcommits.org/)

---

## 附录 A: 与 Screeps 的 API 兼容性

Swarm 不追求与 Screeps API 兼容。设计哲学不同：

- Screeps API 是面向对象的（`creep.moveTo()`, `Game.spawns['Spawn1']`）
- Swarm API 是功能/数据导向的（`move(creep_id, direction)`, return commands）

但可以通过社区项目构建兼容层，将 Screeps 风格 API 调用包装为 Swarm 指令。

## 附录 B: 为什么不用现有 Screeps 方案？

| 关注点 | Screeps | Swarm |
|--------|---------|-------|
| 语言锁定 | 仅 JS | 任意 WASM 语言 |
| 性能上限 | V8 + GC 停顿 | WASM 原生速度 |
| CPU 计量精度 | 墙钟（系统依赖） | Fuel metering（确定性） |
| 确定性 | 不保证 | 设计目标 |
| AI 玩家 | 无 | MCP 原生界面 |
| 代码年代 | 2014 起步，Node.js 8 | 2026，Rust + WASM |
| 许可证 | 混合（server 开源，client 专有） | MIT（完全开源） |

---

*最后更新: 2026-06-14 — Phase 0 Architecture Freeze 确认（R14 终审通过）*
