# pve-spawning

PvE 敌人生成模组。管理中立 NPC 生成、资源潮和公共事件。

## 职责

- 在指定房间按配置生成中立 NPC drone（PvE 威胁）
- NPC 类型：中立 drone（可攻击）、资源潮（大量弱怪）、精英怪（带特殊能力）
- NPC 在指定区域巡逻或守卫
- PvE 击杀掉落资源（通过 Resource Registry）
- PvE 行为由简单的状态机驱动（守卫 → 巡逻 → 追击 → 回归）
- beginner/soft_launch 世界默认 PvE-only，无 PvP

## 依赖

- bevy
- combat-core（NPC 使用同类战斗系统）

## 配置

mod.toml:
```toml
[config]
spawn_interval = { type = "u32", default = 300, min = 10 }
max_npcs_per_room = { type = "u32", default = 50 }
npc_drone_body = { type = "string", default = "[ATTACK, MOVE, MOVE]" }
npc_drop_table = { type = "string", default = "{Energy: 50}" }
```

## 事件

- 写入: `Drone`（NPC 实体）, `NpcAI`（NPC 行为组件）
- 读取: `Position`, `RoomConfig`, `WorldConfig`
