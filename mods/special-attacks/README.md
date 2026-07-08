# special-attacks

特殊攻击模组。管理 8 种特殊 action 的状态机。

## 职责

- 注册 8 种特殊 action handler 到 ActionRegistry（通过 world.toml `vanilla.special_attacks_enabled` 控制）
- Status buffer 系统（S16-S22b），每类 special action 独立 typed buffer：

| Action | Buffer | ID | 效果 |
|--------|--------|-----|-------|
| Hack | HackBuffer | S16 | 夺取敌方 drone 控制权（短时） |
| Drain | DrainBuffer | S17 | 每 tick 吸取目标 Energy |
| Overload | OverloadBuffer | S18 | 增加目标冷却/疲劳 |
| Debilitate | DebilitateBuffer | S19 | 降低目标属性 |
| Disrupt | DisruptBuffer | S20 | 中断持续型动作 |
| Fortify | FortifyBuffer | S21 | 增强己方防御/抗性 |
| Leech | LeechBuffer | S22a | 攻击回血 |
| Fabricate | FabricateBuffer | S22b | 消耗资源生成临时实体 |

- [S22] `status_advance_system` — 唯一 StatusState writer，从 S14 reducer + S16-S22b buffers 读取并统一推进
- 特殊攻击与 HP 伤害互斥——同一 body part 同一 tick 只能执行一种
- 持续型攻击在 drone 移动或被 Disrupt 时中断
- 所有特殊攻击受 `damage_multiplier` 世界规则影响

## 依赖

- bevy

## 配置

world.toml:
```toml
[vanilla]
special_attacks_enabled = ["Hack", "Drain", "Overload", "Debilitate", "Disrupt", "Fortify", "Leech", "Fabricate"]
```
Tutorial/Novice 默认禁用，Standard/Arena 全量启用。

## 事件

- 读取: `ActionRegistry`, `PendingDamage`, `PendingHeal`
- 写入: `HackBuffer`, `DrainBuffer`, `OverloadBuffer`, `DebilitateBuffer`, `DisruptBuffer`, `FortifyBuffer`, `LeechBuffer`, `FabricateBuffer`, `StatusState`
