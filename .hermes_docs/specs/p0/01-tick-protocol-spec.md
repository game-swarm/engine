# P0-1: Tick 协议规范

> **状态**: Phase 2 阻断项

## 1. 状态机

```
                 ┌──────────────────────────────────┐
                 │         空闲等待                   │
                 │        tick_counter = N           │
                 └──────────┬───────────────────────┘
                            │ 到达 tick_interval
                            ▼
                 ┌──────────────────────────────────┐
                 │     阶段一：收集 (COLLECT)          │
                 │  超时: 2500ms                     │
                 │  ┌─────────────────────────┐     │
                 │  │ 对每个活跃玩家:           │     │
                 │  │ 1. 构建可见性快照          │     │
                 │  │ 2. 调用 PlayerExecutor    │     │
                 │  │ 3. 超时 → 空指令列表      │     │
                 │  └─────────────────────────┘     │
                 │  结果: Map<PlayerId, Vec<Cmd>>   │
                 └──────────┬───────────────────────┘
                            │
                            ▼
                 ┌──────────────────────────────────┐
                 │     阶段二：执行 (EXECUTE)          │
                 │  超时: 500ms                      │
                 │  ┌─────────────────────────┐     │
                 │  │ Phase 2a: 命令循环        │     │
                 │  │ 逐条校验 + 逐条应用       │     │
                 │  │ (基于当前 Bevy World)    │     │
                 │  │ Spawn 只校验不入队        │     │
                 │  └─────────────────────────┘     │
                 │  ┌─────────────────────────┐     │
                 │  │ Phase 2b: ECS Systems     │     │
                 │  │ death_mark → spawn →     │     │
                 │  │ combat → regen/decay →   │     │
                 │  │ death_cleanup            │     │
                 │  └─────────────────────────┘     │
                 │  FDB 原子提交（全或无,权威源）   │
                 └──────────┬───────────────────────┘
                            │
                            ▼
                 ┌──────────────────────────────────┐
                 │    阶段三：广播 (BROADCAST)         │
                 │  ┌─────────────────────────┐     │
                 │  │ 1. 计算实体增量            │     │
                 │  │ 2. Dragonfly 缓存更新      │     │
                 │  │ 3. NATS 发布增量           │     │
                 │  └─────────────────────────┘     │
                 └──────────┬───────────────────────┘
                            │ tick_counter = N + 1
                            ▼
                       空闲等待
```

## 2. 阶段一：收集

### 2.1 玩家执行模型

唯一执行器：**WasmSandboxExecutor**。所有玩家的 drone 都通过 WASM 沙箱执行——无论是人类编写还是 AI agent 编写。没有 McpPlayerExecutor。

| 输入来源 | 编译者 | 部署渠道 |
|---------|--------|---------|
| 人类编写代码 | 人类通过 Web UI / CLI 编译 | Web 上传 / `swarm deploy` CLI |
| AI agent 编写代码 | AI 通过自身工具链编译 | MCP `swarm_deploy` |

引擎只关心：「有 WASM 模块了吗？」——不问是谁写的。

### 2.2 收集超时

```
collect_timeout_ms = 2500  // 硬截止时间

在 t + 2500ms 时刻:
  对每个未响应的玩家:
    commands[player] = []   // 宽容失败: 本 tick 无指令
    metrics.collect_timeouts += 1
```

**原则**: 某个玩家卡住不会阻塞整个世界。超时玩家当 tick 指令输出丢弃——不跨 tick 携带（防止 sequence 冲突与跨 tick 重排）。

### 2.3 快照构建

```
fn build_snapshot(player_id, tick) -> Snapshot:
    // all_entities 来自 Bevy World 内存（当前 tick 执行前的权威状态）
    // 不从 FDB/Dragonfly 读 —— COLLECT 阶段不访问外部存储
    entities = visibility_filter(all_entities, player_id, tick)
    return Snapshot {
        tick,
        player_id,
        entities,    // 仅该玩家可见
        terrain,     // 可见地形格
        resources,   // 玩家自身资源
    }
```

快照按房间序列化一次，再按玩家过滤——不是 O(P × E)。

### 2.4 WASM 模块部署

AI 玩家通过 MCP `swarm_deploy` 上传 WASM 模块，引擎在下一 tick 加载新模块：
```
Tick N: 引擎用 WASM 模块 v1 执行玩家代码
Tick N: AI 调用 swarm_deploy，上传 v2
Tick N+1: 引擎自动切换到 v2
```

代码部署不影响当前 tick 执行——当前 tick 使用已加载的模块。切换是原子的。

## 3. 阶段二：执行

### 3.1 指令排序（确定性 + 公平）

**问题**：如果排序 key 是 `(tick_number, player_id, ...)  `，同一个玩家每次都在同一位置——不公平且可被利用。

**方案：种子洗牌 (Seeded Shuffle)**

```rust
// 每 tick 洗牌一次，用 Blake3 XOF 从 seed + tick 派生确定性随机序列
// seed = Blake3(tick_number || world_seed)
// shuffle = Blake3 XOF: for i in 0..N:  position[i] = XOF.read_u64() % (N - i)
let seed = blake3::hash(&[&tick_number.to_le_bytes(), &world_seed]);
let player_order: Vec<PlayerId> = seeded_shuffle(&active_players, &seed);

// 按洗牌后的玩家顺序 + 玩家内部指令序号排序
for (order_index, player_id) in player_order.iter().enumerate() {
    let player_commands = collected_commands[player_id].sort_by_key(|c| c.sequence);
    for cmd in player_commands {
        global_queue.push((order_index, player_id, cmd.sequence, cmd));
    }
}
```

**属性**：
- 确定性：相同 `(tick_number, world_seed, 相同指令集)` → 相同顺序 → 相同世界状态
- 公平性：每个 tick 玩家顺序随机轮换，长期期望均等
- 不可预测：玩家无法提前知道自己在当前 tick 的排序位置

### 3.2 资源竞争 (Resource Contention)

**场景**：两个玩家的 drone 在同一 tick 试图采集同一个 Source。

**规则：按排序顺序依次执行，先到先得。**

```
Source E1: energy = 5

排序后指令队列:
  1. Player B: harvest(E1) → 拿走 5，E1 剩余 0
  2. Player A: harvest(E1) → 校验时发现 E1.energy = 0
     → RejectionReason: SourceEmpty
     → 记录到 TickTrace
```

**应用范围**：
| 竞争类型 | 处理方式 |
|---------|---------|
| 采集同一 Source | 先到先得，耗尽后 `SourceEmpty` |
| 建造同一坐标 | 先到先得，坐标被占后 `TileOccupied` |
| 攻击同一目标 | 全部执行——多个攻击者可以打同一目标 |
| 治疗同一目标 | 按顺序加血，满血后 `AlreadyFullHealth` |
| 传输资源到同一目标 | 顺序填充，容量满后 `TargetFull` |

**设计意图**：
- 先到先得简单、确定、可解释
- 种子洗牌保证了「先到」的公平性——长期来看每个玩家都有同等概率先到
- 创造了策略深度：要不要多个 drone 采集同一个源？万一排在后面就浪费指令
- 不采用比例分配（太复杂且失去竞争性），不采用价高者得（需要市场机制，超出入门复杂度）

### 3.3 指令执行模型（Inline）

命令循环采用 **Inline 模型**：逐条校验 + 逐条应用，校验基于**当前** Bevy World 状态（非快照）。Move/Harvest/Build/Transfer/Attack/Heal/Recycle 在命令循环中立即执行。Spawn 命令在 Phase 2a 中只校验不入队，在 Phase 2b spawn_system 中统一创建。

非法指令 → 拒绝，记录 RejectionReason，写入 TickTrace。

### 3.4 ECS 系统执行顺序 (Bevy)

Phase 2b 中 ECS Systems 按 `.chain()` 严格排序：

```rust
app.add_systems(Update, (
    death_mark_system,       // 标记待死亡 entity，释放 room cap 槽位
    spawn_system,            // 统一创建 Phase 2a 校验通过的 drone
    regeneration_system,     // 资源点再生
    combat_system,           // 战斗结算（damage 先 → heal 后）
    decay_system,            // 疲劳/冷却递减
    death_cleanup_system,    // 实际 despawn 已标记 entity
).chain());
```

`.chain()` 强制串行执行 → 确定性。后续优化用 `.before()/.after()` 实现部分并行同时保持正确性。

### 3.5 Tick 原子性

整个阶段二包裹在 FoundationDB 事务中：

```
txn = fdb.create_transaction()
for command in sorted_commands:
    result = validate_and_apply(txn, command, world_state)
    if result.is_err():
        record_rejection(txn, command, result)
txn.set("/tick/{tick}/complete", true)
txn.commit()  // 全提交 或 全回滚
```

`txn.commit()` 失败（冲突/网络）→ 最多重试 3 次 → 全部失败则 tick 放弃。
放弃的 tick：世界状态不变，tick_counter 不递增，消耗的 CPU fuel 退还玩家。
放弃后等待 1s 重试同一 tick（避免立即重试导致相同的 FDB 冲突）。
连续放弃 3 次 → 引擎进入降级模式（暂停新玩家加入），告警触发。
**关键**: EXECUTE 开始时对 `Bevy World` 做内存快照——FDB rollback 不自动恢复 Bevy 状态，需显式 `world.restore(snapshot)`。

## 4. 阶段三：广播

### 4.1 增量计算

```
delta = compute_delta(world_state_before, world_state_after)
// delta 仅包含本 tick 变更的实体
```

### 4.2 持久化 → 缓存 → 发布

```
1. Read committed tick result from in-memory post-commit state or FDB versionstamp
2. Dragonfly.update(delta)   // 非权威缓存，允许滞后。失败则从 FDB 重建
3. NATS.publish("tick.{tick}", delta)  // 网关 → WebSocket 客户端
```

**BROADCAST failure never rolls back committed tick**——tick 已在 EXECUTE 阶段持久化到 FDB。BROADCAST 阶段的任何失败（Dragonfly 未命中、NATS 断开、部分客户端未收到）都不影响世界状态。客户端通过 `last_tick` 字段检测 gap → 主动 fetch。

## 5. Tick 健康指标

| 指标 | 阈值 | 动作 |
|------|------|------|
| `collect_timeout_rate` | > 10% 玩家 | 告警：太多慢执行器 |
| `tick_abandon_rate` | > 0 | 严重：FDB 提交失败 |
| `tick_duration_p99` | > 2800ms | 警告：接近 3s 目标 |
| `command_rejection_rate` | > 20% 每玩家 | 标记玩家审查 |

## 6. Tick Failure Semantics — 失败语义

### 6.1 失败模式矩阵

| 失败点 | 触发条件 | 对本 tick 影响 | 对玩家影响 | 恢复策略 |
|--------|---------|--------------|-----------|---------|
| **WASM timeout** | 玩家 tick() 超过 collect_timeout_ms (2500ms) | 该玩家 0 指令，其他玩家正常 | 空 tick，不退 fuel | 下 tick 正常执行 |
| **WASM crash** | 玩家 WASM 崩溃/panic/OOM | 同上 | 空 tick，不退 fuel。连续 3 tick crash → 玩家标记 degraded | 自动恢复，degraded 需人工解除 |
| **WASM output invalid** | tick 输出不符合 JSON schema（见 P0-2 §1.1） | 该玩家所有指令丢弃 | 空 tick，不退 fuel | 下 tick 正常（需玩家修复代码） |
| **FDB commit fail** | FoundationDB 事务冲突/网络错误 | tick 放弃（state 不变，tick_counter 不递增） | CPU fuel 退还 | 重试 3 次，失败等 1s 重试同 tick。连续 3 tick abandon → 引擎降级 |
| **Dragonfly cache miss** | 缓存未命中/过期 | 无——回退到 FDB 直读 | 无影响 | 从 FDB 重建缓存（异步） |
| **Dragonfly cache stale** | 缓存版本落后于 FDB | 无——FDB 为权威源 | 旧数据给查询入口，不影响 tick | 下次写入时自动刷新 |
| **NATS publish fail** | NATS 连接断开/超时 | tick 结果已持久化到 FDB，但客户端未收到 delta | 客户端未更新，需等 polling fallback | NATS 重连；客户端 5s 未收到 delta → 主动拉取 |
| **Broadcast partial** | 部分客户端已收到 delta，部分未收到 | 客户端间状态不一致（暂时） | 未收到的客户端显示旧状态 | 客户端通过 last_tick 字段检测 gap → 主动 fetch |
| **TickTrace write fail** | FDB 写入 TickTrace 失败（磁盘满） | tick 执行完成但审计日志不完整 | 无 gameplay 影响 | 告警；TickTrace 丢失的 tick 标记为不可回放 |

### 6.2 降级模式 (Degraded Mode)

连续 3 次 tick abandon → 引擎进入降级模式：
- 暂停新玩家加入 (`join_lock = true`)
- 暂停 MCP_Deploy 来源（禁止代码更新，防部署丢失）
- 保持已有玩家 WASM 执行
- 告警升级 → 需管理员介入
- 连续 10 tick 正常 → 自动退出降级模式

### 6.3 回放协议

#### 6.3.1 记录

每个 tick 写入 FDB（不可变）：
```
/tick/{N}/commands   → 全部玩家排序后的 RawCommand
/tick/{N}/state      → tick 后的完整世界状态
/tick/{N}/rejections → 被拒绝的指令及原因
/tick/{N}/metrics    → TickMetrics
```

AI 玩家：记录 ACCEPTED 指令，不是原始 LLM 输出。回放时喂记录指令——不重调 LLM。

#### 6.3.2 回放执行

```
fn replay_tick(tick_N) -> WorldState:
    state = load_state(tick_N - 1)     // 起始状态
    commands = load_commands(tick_N)   // 记录的指令
    return execute_deterministic(state, commands)  // 必须 == 记录状态
```

`execute_deterministic(state, commands) != recorded_state` → 确定性 BUG。

#### 6.3.3 Wasmtime 版本与回放共存

**问题**: `wasmtime = "=30.0"` 锁定版本 → 发现 CVE 升级后旧 tick 回放中断。

**策略**: TickTrace 始终记录 `Command[]` 而非 WASM 输出。回放时引擎直接执行已记录的指令序列，不重新调用 WASM。Wasmtime 版本变更不影响回放。仅当 tick 被标记为"降级模式"（WASM 执行异常）时，需匹配 Wasmtime 版本进行二次回放验证。

#### 6.3.4 Tick Boundary Contract

COLLECT 阶段从 Bevy World 内存读取权威状态，不访问 FDB/Dragonfly。EXECUTE 阶段在 Bevy World 上原地修改 → FDB 事务提交 → 成功后 FDB 为新的权威源。Bevy World 与 FDB 的关系：Bevy 是每 tick 的工作副本，FDB 是持久化的权威源。启动/恢复时从 FDB 重建 Bevy World。
