# resource-decay

资源衰减模组。掉落资源随 tick 衰减，防止无限堆积。

## 职责

- 每 tick 对地面掉落的 Resource 实体按 `decay_rate_ppm` 衰减
- 衰减仅影响非存储中的资源（地面掉落物）
- Storage/Extension/Terminal 中的资源不衰减
- 不同资源类型可配置不同衰减率
- resource ttl：衰减至 0 后自动 despawn

## 依赖

- bevy

## 配置

mod.toml:
```toml
[config]
decay_rate_ppm = { type = "u32", default = 1000, min = 0, max = 100000 }
```

ppm = parts per million per tick。1000 ppm = 每 tick 衰减 0.1%。

## 事件

- 读取: `Resource`（amounts）, `StructureType`（存储中则跳过）
- 写入: `Resource.amounts`（衰减后）, `DeathMarker`（0 时）
