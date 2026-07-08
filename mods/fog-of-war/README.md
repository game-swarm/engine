# fog-of-war

战争迷雾模组。管理每玩家可见性过滤。

## 职责

- 默认每 tick 构建 visibility snapshot 时按 `is_visible_to(caller_player_id)` 过滤实体
- 玩家 drone 视野范围：由 Controller RCL + Observer 建筑决定
- RCL 1-4：默认视野范围（当前房间 + 相邻房间，最多 9 房间）
- RCL 5+（Observer）：范围扩展（+1 房间/级）
- 所有 host function 调用返回值也经 is_visible_to 过滤
- fog_of_war = false 时（Tutorial 世界）全图可见
- 每个玩家看到自己的可见性快照，其他玩家不可见

## 依赖

- bevy

## 配置

world.toml:
```toml
[visibility]
fog_of_war = true
```

## 事件

- 写入: `VisibilityMap`（每玩家每 tick）
- 读取: `Position`, `Drone`, `Structure`, `Controller`, `PlayerId`
