# combat-core

战斗核心模组。管理所有 combat intent 的产生、归约和伤害应用。

## 职责

- 注册 3 种基础 combat action handler（Attack, RangedAttack, Heal）到 ActionRegistry
- [S11] `attack_system` — 近战攻击：读取 PendingDamage(Kinetic) → 按 body part 计算伤害 → 写入 damage intent
- [S12] `ranged_attack_system` — 远程攻击：读取 PendingDamage(根据武器类型) → 射程/弹药检查 → 写入 damage intent
- [S13] `heal_system` — 治疗：读取 PendingHeal → 按 HEAL body part 恢复 HP
- [S14] `special_attack_reducer` — 从 action handler status intent buffer 读取 → merge → resolve sort → 交付 S22
- [S15] `damage_application_system` — 统一将 damage intent 写入 Entity.hits（含抗性计算）
- Tower 自动攻击：读取 Tower 配置 → 射程内敌方 → 生成 PendingDamage
- 注册伤害类型：Kinetic, Thermal, EMP, Sonic, Corrosive, Psionic
- 注册 body part：ATTACK(伤害), RANGED_ATTACK(远程), HEAL(治疗), TOUGH(减伤)

## 依赖

- bevy

## 配置

world.toml:
```toml
[combat]
pvp_enabled = true
friendly_fire = false
damage_multiplier_bp = 10000
```

## 事件

读取: `PendingDamage`, `PendingHeal`, `ActionRegistry`, `DamageType`
写入: `Entity.hits`, `StatusState`
