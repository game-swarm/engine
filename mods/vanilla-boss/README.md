# vanilla-boss

Boss NPC 遭遇战模组。管理世界 Boss 和 Arena Boss。

## 职责

- 世界 Boss：在指定房间周期性生成，全服玩家可挑战
- Arena Boss：Arena 模式中按轮次出现
- Boss 特性：多阶段（phases）、血条分段、特殊掉落
- Boss 掉落：击杀后掉落稀有资源/物品
- Boss AI：阶段状态机（Phase 1: 普通攻击 → Phase 2: 范围攻击 → Phase 3: 狂暴）
- Boss 生成定时器：在 world.toml 配置中声明

## 依赖

- bevy
- combat-core（使用同类 combat/damage 系统）
- serde

## 配置

world.toml:
```toml
[mods.config]
world_bosses_enabled = true
arena_bosses_enabled = true
boss_spawn_interval = 5000
```

## 事件

- 读取: `WorldConfig`, `Tick`
- 写入: `Drone`（Boss 实体）, `BossAI`（阶段状态机）, `Resource`（掉落物）
