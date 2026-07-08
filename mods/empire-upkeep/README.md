# empire-upkeep

帝国规模维护费模组。drone 和房间越多，每 tick 消耗越大。

## 职责

- 每 tick 按玩家拥有的 drone 数和房间数计算维护费
- 计算公式：`cost = drone_count × drone_cost + room_count × room_base + room_count² × room_superlinear`
- 从玩家 Energy 储备中扣除（通过 Resource Ledger）
- 资源不足时按 onshortfall 策略处理：degrade（降级 Controller）/ damage（伤害 drone）/ despawn（杀死最旧 drone）
- 构建 anti-snowball 经济曲线——维护费随帝国规模超线性增长

## 依赖

- bevy
- base-economy（通过 Resource Ledger 扣费）

## 配置

mod.toml:
```toml
[config]
drone_cost = { type = "u32", default = 2, min = 0, max = 100 }
room_base = { type = "u32", default = 10, min = 0, max = 1000 }
room_superlinear = { type = "fixed<u32,4>", default = 1, min = 0, max = 100 }
onshortfall = { type = "enum", default = "degrade", values = ["degrade", "damage", "despawn"] }
```

## 资源

- 维护费从玩家 Energy 储备中扣除
- 短fall 处理通过 Entity 操作（降级、伤害、杀死）
