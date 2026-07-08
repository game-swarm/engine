# depot-storage

前线仓库（Forward Depot）物流模组。提供本地化维修和资源供给。

## 职责

- Forward Depot 实体：独立 Structure 类型，带本地资源存储
- 维修服务：drone 移动到 Depot range=1 内 → 消耗 Depot 本地资源 → 降低 drone age
- 队列管理：每个 Depot 有 repair_capacity / tick，drone 按确定性队列排序
- 相邻格只有 6 个 → 大量 drone 需要排队，形成物流拥挤决策
- 资源不足时本 tick 停止维修
- 没有全局 repair cap/cost——只受物理范围、设施容量、队列限制

## 依赖

- bevy

## 配置

mod.toml:
```toml
[config]
repair_range = { type = "u32", default = 1 }
repair_capacity = { type = "u32", default = 5 }
depot_hits = { type = "u32", default = 5000 }
depot_capacity = { type = "u32", default = 10000 }
```

## 资源

- 消耗 Depot 本地存储的资源维修 drone
- 资源通过 Transfer 指令由 drone 供给
