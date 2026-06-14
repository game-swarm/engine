# P0-7: World Rules Engine — 世界规则配置规范

> **状态**: Phase 1 设计基础 | **关联**: DESIGN.md §8

## 1. 定位

Swarm 不是「一个游戏」，是「游戏引擎平台」。规则模组是**可安装的 Rhai 脚本 + 声明式配置**——轻量、确定、可组合。

```
玩家代码:  WASM → 控制 drone     (不可信 → sandbox)
规则模组:  Rhai → 修改世界规则    (服主声明 → 引擎嵌入)
引擎核心:  Rust → 确定性模拟      (不可变)
```

每个世界通过 `world.toml` 启用一组模组，每模组有独立的参数配置。模组通过 `actions` 请求引擎操作——不能绕过 Command Validation Pipeline。

## 2. 配置 Schema

```toml
# world.toml

[world]
name = "World of Swarm"
mode = "persistent"               # persistent | arena
tick_interval_ms = 3000

[spawn]
policy = "RandomRoom"
respawn = "NewRoom"
cooldown = 0

[code]
update_cost = {}
update_cooldown = 0
update_window = { every = 0, duration = 0 }
propagation_speed = 0
propagation_source = "Spawn"

# ═════════════════════════════════════
# 自定义资源类型
# ═════════════════════════════════════

[[resource_types]]
name = "Energy"
display_name = "能量"
category = "energy"
starting_amount = 1000
max_storage = 100000
decay_rate = 0.0
tradeable = true

[[resource_types]]
name = "Matter"
display_name = "物质"
category = "mineral"
starting_amount = 500
max_storage = 50000
decay_rate = 0.001
tradeable = true

# 各动作资源消耗（键名来自 resource_types.name）
[actions.costs]
spawn = { Energy = 200, Matter = 50 }
build.Extension = { Energy = 50 }
build.Tower = { Energy = 100, Matter = 25 }
body_part.Move = { Energy = 50 }
body_part.Work = { Energy = 100 }
body_part.Attack = { Energy = 80, Matter = 20 }
body_part.Heal = { Energy = 250, Matter = 100 }
code_update = { Energy = 500 }

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

# ═════════════════════════════════════

[drone]
env_vars = true
memory_size = 1024
memory_spawn_cost = {}          # 每 byte 孵化成本
memory_upkeep_cost = {}         # 每 byte 每 tick 维护费
max_body_parts = 50
max_drones_per_player = 500

[resources]
source_regeneration_rate = 10000   # fixed<u32,4>: 1.0
build_cost_multiplier = 10000       # fixed<u32,4>: 1.0
drone_decay_rate = 10000            # fixed<u32,4>: 1.0

# 物流配置
global_storage_enabled = true
transfer_to_global_cost = { Energy = 0.01 }
transfer_from_global_cost = { Energy = 0.05 }
market_requires_terminal = true

[combat]
pvp_enabled = true
friendly_fire = false
damage_multiplier = 1.0

[visibility]
fog_of_war = true

# ═════════════════════════════════════
# 已安装模组
# ═════════════════════════════════════

[[mods]]
name = "empire-upkeep"
version = "1.2.0"
[mods.config]
drone_cost = 5
room_superlinear = 2    # fixed<u32,4>: 0.0002
onshortfall = "damage"

[[mods]]
name = "resource-decay"
version = "0.3.0"
[mods.config]
decay_rate = 0.001

```

## 3. ECS Plugin 注册

```rust
// engine/src/world_rules.rs

pub struct WorldConfig {
    pub world: WorldSettings,
    pub spawn: SpawnConfig,
    pub code: CodeConfig,
    pub drone: DroneConfig,
    pub resources: ResourceConfig,
    pub combat: CombatConfig,
    pub visibility: VisibilityConfig,
}

impl WorldConfig {
    /// 根据配置注册可选的 ECS System
    pub fn register_systems(&self, app: &mut App) {
        // 注入资源注册表——核心组件
        let registry = ResourceRegistry::from_config(self);
        app.insert_resource(registry);

        // 基础系统始终注册（Phase 2b: Inline 命令执行后的系统链）
        app.add_systems(Update, (
            death_mark_system,       // 标记待死亡 entity，释放 room cap
            spawn_system,            // 统一创建校验通过的 drone
            regeneration_system,     // 资源点再生
            combat_system,           // 战斗结算（damage 先 → heal 后）
            decay_system,            // 疲劳/冷却递减
            death_cleanup_system,    // 实际 despawn
        ).chain());

        // === 孵化规则 ===
        if self.spawn.policy == SpawnPolicy::ManualSelect {
            app.add_systems(Update, manual_spawn_system.before(spawn_system));
        }

        // === 代码部署规则 ===
        if self.code.update_cost != ResourceCost::default() {
            app.add_systems(Update, code_update_cost_system);
        }
        if self.code.update_window.every > 0 {
            app.add_systems(Update, code_update_window_system);
        }
        if self.code.propagation_speed > 0 {
            app.add_systems(Update, code_propagation_system.before(spawn_system));
        }

        // === Drone 控制 ===
        if self.drone.env_vars {
            app.add_systems(Update, drone_env_var_system);
        }
        if !self.drone.memory_upkeep_cost.is_empty() {
            app.add_systems(Update, memory_upkeep_system.before(decay_system));
        }

        // === 可见性 ===
        if !self.visibility.fog_of_war {
            // 无 fog of war → 跳过可见性过滤
            app.insert_resource(VisibilityMode::FullInformation);
        }

        // === 战斗 ===
        if !self.combat.pvp_enabled {
            app.add_systems(Update, pvp_block_system.before(combat_system));
        }
    }
}
```

## 4. 规则 System 示例

### 代码传播速度

```rust
/// 当 code_propagation_speed > 0 时，代码更新从传播源向外扩散
fn code_propagation_system(
    config: Res<WorldConfig>,
    mut drones: Query<(Entity, &Position, &Owner, &mut CodeVersion)>,
    spawns: Query<(&Position, &Owner), With<Spawn>>,
) {
    let speed = config.code.propagation_speed;
    if speed == 0 { return; }  // 即时传播，跳过后面的计算

    for (entity, pos, owner, mut version) in drones.iter_mut() {
        // 找到该玩家最近的传播源
        let nearest_source = spawns.iter()
            .filter(|(_, o)| o.0 == owner.0)
            .map(|(p, _)| distance(pos, p))
            .min();

        if let Some(dist) = nearest_source {
            // 计算传播延迟：距离 / 速度 = tick 数
            let propagation_delay = dist / speed;
            // 如果版本太新还没传播到，保持旧版本
            if version.updated_at + propagation_delay > current_tick() {
                version.fallback_to_previous();
            }
        }
    }
}
```

### 内存维护费

```rust
/// 当 memory_upkeep_cost 不为空时，每 tick 按使用量扣资源
fn memory_upkeep_system(
    config: Res<WorldConfig>,
    mut players: Query<(&mut PlayerResources, &PlayerMemory)>,
) {
    let upkeep = &config.drone.memory_upkeep_cost;
    if upkeep.is_empty() { return; }

    for (mut resources, memory) in players.iter_mut() {
        let used_bytes = memory.used_bytes();
        for (res_name, cost_per_byte) in upkeep {
            let total_cost = (used_bytes * cost_per_byte) / FIXED_SCALE;
            if total_cost > 0 {
                resources.deduct(res_name, total_cost);
                // 资源不足 → drone 随机失忆（减少存储）
                if resources.get(res_name) < 0 {
                    memory.truncate_to_fit(resources);
                }
            }
        }
    }
}
```

## 5. WASM 侧 API

```rust
// host function: 读取世界配置
fn host_get_world_config(key_ptr: i32, key_len: i32, out_ptr: i32, out_len: i32) -> i32;
```

```typescript
// TypeScript SDK
interface WorldConfig {
    spawn: { policy: string; respawn: string; cooldown: number };
    code: {
        update_cost: Record<string, number>;
        update_cooldown: number;
        update_window: { every: number; duration: number };
        propagation_speed: number;
    };
    drone: { env_vars: boolean; memory_size: number };
    combat: { pvp_enabled: boolean; friendly_fire: boolean };
}

// 用法
const cfg = Game.world.config();

// 根据规则调整策略
if (cfg.code.propagation_speed > 0) {
    // 分阶段部署：先更新近处 drone，再逐步扩散
    deployByDistance(cfg.code.propagation_speed);
}

if (cfg.code.update_window.every > 0) {
    // 在窗口期内批量更新
    scheduleUpdate(cfg.code.update_window);
}

if (cfg.drone.env_vars) {
    // 使用环境变量做角色标注
    drone.set("role", "harvester");
}

if (cfg.drone.memory_upkeep_cost.Energy > 0) {
    // 内存有维护费——只在必要时存储状态
    drone.memory.compact();
}
```

## 5.1 Rhai 事务性执行模型

规则模组的 Rhai 脚本在每 tick 的规则注入阶段执行。所有 `actions.*` 调用（如 `actions.deduct`、`actions.award`、`actions.emit_event`）**不直接修改世界状态**，而是遵循事务性语义：

```
Rhai 脚本执行
    │
    ▼
┌─────────────────────────────────┐
│  RhaiActionBuffer (内存缓存)     │  ← 所有 actions.* 调用写入此 buffer
│  - deducts: Vec<DeductAction>   │
│  - awards:  Vec<AwardAction>    │
│  - events:  Vec<GameEvent>      │
│  - effects: Vec<WorldEffect>    │
└────────────┬────────────────────┘
             │ 脚本执行完毕
             ▼
┌─────────────────────────────────┐
│  钩子执行完毕检查                 │  ← 所有注册的 Rhai 钩子均已返回
│  (on_tick / on_command / etc.)  │
└────────────┬────────────────────┘
             │ 全部成功
             ▼
┌─────────────────────────────────┐
│  统一 Apply                      │  ← 按顺序将 buffer 内容写入世界状态
│  1. deduct（扣资源）              │     FDB 事务内 atomic commit
│  2. award（发资源）               │
│  3. emit_event（发事件）          │
│  4. effect（世界效果）            │
└────────────┬────────────────────┘
             │
             ▼
       世界状态已更新
```

**超时回滚**：若任一 Rhai 脚本超过墙钟预算（默认 100ms），整个 `RhaiActionBuffer` 丢弃，世界状态不变。墙钟预算仅作为安全网，隔离脚本副作用——一个脚本超时不影响其他脚本或核心引擎。

**部分失败处理**：
- 单个 `actions.*` 调用失败（如 deduct 资源不足）→ 该 action 被跳过，不影响 buffer 中其他 action
- 脚本 panic / 语法错误 → 该脚本的全部 buffer 丢弃，其他脚本 buffer 保留
- 所有脚本执行完毕后，buffer 中有效的 action 一次性 apply

**隔离保证**：
- Rhai 脚本**不能**绕过 Command Validation Pipeline（见 P0-2 §1）
- Rhai 脚本**不能**直接写入 ECS 组件——只能通过 `actions.*` API
- Rhai 脚本**不能**访问其他玩家的私有数据
- Buffer apply 阶段由引擎核心在 FDB 事务中执行，保证确定性

## 7. World vs Arena 默认值

| 规则 | World | Arena |
|------|-------|-------|
| `spawn.policy` | RandomRoom | FixedSpawn |
| `code.update_cost` | {} | {} |
| `code.update_window` | 无限制 | 赛前锁定 |
| `code.propagation_speed` | 0 | 0 |
| `drone.env_vars` | true | true |
| `combat.pvp_enabled` | true | true |
| `visibility.fog_of_war` | true | false（全场可见） |

## 8. 配置校验

```rust
fn validate_config(config: &WorldConfig) -> Result<(), Vec<String>> {
    let mut errors = vec![];

    if config.tick_interval_ms < 1000 { errors.push("tick_interval_ms too short"); }
    if config.code.propagation_speed > 0 && config.code.propagation_speed > 100 {
        errors.push("propagation_speed too high");
    }
    if config.drone.memory_size > 65536 {
        errors.push("memory_size exceeds 64KB");
    }
    if config.combat.damage_multiplier < 1 {
        errors.push("damage_multiplier must be positive");
    }

    if errors.is_empty() { Ok(()) } else { Err(errors) }
}
```

## 9. 与核心引擎的边界

核心引擎**不知道规则的存在**。规则 System 是外挂的：

```
核心引擎职责:
  - Tick 调度 (Collect → Execute → Broadcast)
  - Command 校验与执行
  - ECS 基础 Systems (移动、采集、战斗、死亡)
  - 确定性保证
  - 持久化

规则 System 职责:
  - 在执行前后附加逻辑
  - 不修改核心引擎代码
  - 按配置启用/禁用
```

规则 System 只能：
1. 在 Command 执行**前**拦截（如代码传播检查）
2. 在 Command 执行**后**补充（如手动控制追加）
3. 修改 ECS 资源/组件（如传播系统修改 CodeVersion）
4. 绝不可绕过 Command 校验管线
