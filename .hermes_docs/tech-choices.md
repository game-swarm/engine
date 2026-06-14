# Swarm 技术选型

## 1. 引擎框架: Rust + Bevy ECS

### 备选

| 方案 | 语言 | ECS | 优势 | 劣势 |
|------|------|-----|------|------|
| Bevy | Rust | ✅ 原生 | `.chain()` 天然适配确定性排序；活跃社区；纯 Rust 无 FFI | 相对年轻，API 仍在小版本变动 |
| Legion | Rust | ✅ 原生 | 更成熟稳定 | 已归档，不再维护 |
| Flecs | C | ✅ 原生 | 最快的 ECS；C99 可嵌入任何语言 | FFI 开销；Rust 绑定非一等公民 |
| Unity DOTS | C# | ✅ 原生 | 成熟编辑器 | 闭源；不是 headless 设计；许可费用 |
| 自研 ECS | Rust | ✅ | 完全控制 | 重复造轮子；需要自己解决调度、并行、查询优化 |
| Godot + Rust | GDScript/C++ | ❌ | 免费开源 | 不是真正的 ECS；确定性和 headless 不够好 |

### 选择: Bevy

`.chain()` 强制系统串行执行，和 Determinism Contract（§8.8）完美匹配。纯 Rust 无 FFI，WASM 沙箱和 Bevy 共享同一 allocator。社区活跃度是 Rust 游戏引擎中最高的。

---

## 2. 玩家沙箱: WASM + Wasmtime

### 备选

| 方案 | 类型 | 优势 | 劣势 |
|------|------|------|------|
| Wasmtime | 独立运行时 | fuel metering 原生；epoch interruption；pinnable version；C API | 相对重（依赖 Cranelift JIT） |
| Wasmer | 独立运行时 | 多编译器后端 (LLVM/Cranelift/Singlepass) | fuel metering API 不如 Wasmtime 成熟 |
| WasmEdge | 独立运行时 | 云原生定位；轻量 | fuel metering 不原生支持，需自己实现 |
| V8 Isolate | JS 引擎 | 最快的 WASM JIT | fuel metering 不支持；embedding API 复杂；不是为沙箱游戏设计的 |
| Docker/gVisor | 容器 | 隔离性最强 | 启动慢（不能 per-tick fork）；不可确定 |

### 选择: Wasmtime

三个硬需求决定了选择：(1) fuel metering 原生支持——能精确计费每 tick 的 CPU 消耗；(2) epoch interruption——超时即杀，配合 2500ms 硬截止；(3) per-tick fork 生命周期——每 tick 新 fork，执行完 kill，tick 间无状态保留。这三个在 Wasmtime 中是一等公民 API，其他备选需要自己实现至少一项。

---

## 3. 模组脚本: Rhai

### 备选

| 方案 | 类型 | 优势 | 劣势 |
|------|------|------|------|
| Rhai | Rust 嵌入式 | 无 C 依赖；AST 解释（确定性天然）；极简语法 | 性能不及 JIT 方案 |
| Lua (mlua) | C 嵌入 | 生态最大；人人会写 | C 依赖；JIT (LuaJIT) 非确定性；GC 暂停不可控 |
| WASM | 同玩家沙箱 | 统一 runtime | 重：每次加载模组需要编译 WASM；模组是小脚本，不值得 |
| Python (PyO3) | C 嵌入 | 生态最大 | GIL；启动慢；确定性差；太重 |
| JavaScript (boa/deno_core) | Rust/JS | 通用语言 | GC 非确定；太重 |

### 选择: Rhai

三层信任模型（WASM 不可信 → Rhai 服主信任 → Rust 核心不可变）中，Rhai 处于中间层。关键优势：(1) 纯 Rust，无 C 依赖，和引擎共享编译目标；(2) AST 解释天然确定——同一引擎版本的同一脚本产生完全相同的结果；(3) 极简语法降低服主创作门槛——不需要编译、不需要外部工具链；(4) 可关闭浮点引擎侧，与 Determinism Contract 一致。

---

## 4. 持久化: FoundationDB

### 备选

| 方案 | 事务模型 | 优势 | 劣势 |
|------|---------|------|------|
| FoundationDB | 严格可序列化 | 真正的 ACID；每 tick 原子提交天然适配 | 运维复杂度高（需要 cluster）；Rust 绑定不如 SQL |
| SQLite | 可序列化 | 零运维；单文件 | 单写入者；不适配多房间/多 shard 扩展 |
| PostgreSQL | 可重复读 | 生态最强 | 不是严格可序列化（默认）；每 tick 提交在 MVCC 下有写放大 |
| RocksDB | 快照隔离 | 极快写入 | 不是严格可序列化；无跨 key 事务保证 |
| TiKV | 分布式 KV | 水平扩展 | 事务模型弱于 FDB；运维复杂度相当 |

### 选择: FoundationDB

每 tick 需要原子提交一个包含"全部玩家指令执行结果"的事务——如果部分成功部分失败，世界状态就不可回放。FoundationDB 的严格可序列化是唯一在分布式 KV 中提供这个保证的。Simulation testing（FDB 内置的确定性模拟测试框架）和 Swarm 的 replay determinism 是同一种哲学——这不仅仅是技术选择，是理念一致。

---

## 5. 实时推送: NATS

### 备选

| 方案 | 模式 | 优势 | 劣势 |
|------|------|------|------|
| NATS | Pub/Sub | 极轻量（单二进制）；Go 原生；支持 WebSocket | 无持久化队列（JetStream 是附加组件） |
| Kafka | 日志 | 最成熟的持久化队列 | 运维重（ZooKeeper/KRaft）；对 tick 推送是杀鸡用牛刀 |
| Redis Pub/Sub | Pub/Sub | 已部署 Dragonfly 可复用 | 无持久化；断线丢消息 |
| WebSocket 直连 | P2P | 零中间件 | 需要自己管理连接状态和重连 |

### 选择: NATS

tick delta 的特点：(1) 每 3s 推一次，数据量小；(2) 错过可以回放（客户端检测 gap → fetch），不需要持久化保证；(3) 需要多客户端广播。NATS 的轻量设计完美匹配这个场景。Kafka 是为"每 tick 产生百万事件还要留存 7 天"设计的，Swarm 不需要。

---

## 6. 热缓存: Dragonfly

### 备选

| 方案 | 类型 | 优势 | 劣势 |
|------|------|------|------|
| Dragonfly | Redis 兼容 | 多线程 ~1M QPS；Redis 协议兼容 | 相对新 |
| Redis | 单线程 | 最成熟；生态最大 | 单线程瓶颈 |
| KeyDB | Redis fork | 多线程 | 社区小；维护不确定 |
| Garnet | 微软 .NET | 极快 | 非 Redis 完全兼容；.NET 依赖 |
| 进程内缓存 | 无 | 零延迟 | 多引擎进程需要共享；重启丢失 |

### 选择: Dragonfly

角色是"非权威缓存"——FDB 是权威源，Dragonfly 只是加速读取。需要的就是快 + Redis 协议兼容（生态工具顺手可用）。Dragonfly 的多线程设计在这个场景下比 Redis 单线程优势明显——当 500 个 WebSocket 连接同时请求当前 tick 状态时。

---

## 7. 分析: ClickHouse

### 备选

| 方案 | 类型 | 优势 | 劣势 |
|------|------|------|------|
| ClickHouse | 列式 OLAP | tick 级时序查询是天然主场 | 不适合事务型查询 |
| TimescaleDB | PostgreSQL 扩展 | SQL 兼容；成熟 | 性能不及 ClickHouse |
| InfluxDB | 时序 | 专为时序设计 | 查询语言非标准 SQL |
| Prometheus | 指标 | 运维简单 | 不是为 tick 级高基数数据设计的 |
| 直接用 FDB | KV | 零额外组件 | tick 级聚合查询性能差 |

### 选择: ClickHouse

每个 tick 产出 `TickMetrics`（每玩家的 fuel 消耗、拒绝率、deltas）。需要回答"过去 1000 tick 中谁的 Room 扩张最快"。这是列式 OLAP 的经典场景——ClickHouse 在这里没有任何对手。

---

## 8. 哈希 / PRNG / 代码签名: Blake3（单原语）

### 备选

**哈希**:

| 方案 | 速度 | 生态 | 选择理由 |
|------|------|------|---------|
| Blake3 | ~6 GB/s | Rust 一等 | **选择** |
| SHA-256 | ~0.5 GB/s | 最广泛 | 慢 12×，没有优势 |
| SHA-512/256 | ~0.3 GB/s | 广泛 | 更慢 |

**PRNG**:

| 方案 | 速度 | 安全性 | 选择理由 |
|------|------|------|---------|
| Blake3 XOF | ~6 GB/s | Blake3 级 | **选择** — 与哈希同原语 |
| ChaCha12 | ~3 GB/s | 2^128 | 快但需额外依赖 |
| ChaCha20 | ~2 GB/s | 最高 | 慢，需额外依赖 |
| AES-256-CTR | ~5/0.15 GB/s | 最高 | 无 AES-NI 时退化 30×，不可用于跨平台 |

**代码签名**:

| 方案 | 签名大小 | 速度 | 选择理由 |
|------|---------|------|---------|
| Blake3 MAC | 32 B | ~6 GB/s | **选择** — 与哈希/PRNG 同原语 |
| Ed25519 | 64 B | 快 | 用于证书签发（标准非对称） |

### 选择: Blake3 全覆盖

技术栈中有三个独立需求刚好被同一个原语覆盖：哈希（Blake3）、确定随机数（Blake3 XOF）、代码签名（Blake3 keyed hash / MAC）。统一为 Blake3 后：(1) 依赖栈减少一个 crate（ChaCha）；(2) 审计面减半；(3) 纯软件 ~6 GB/s，无平台退化；(4) seed+offset XOF 模式天然适配 per-player per-tick 的确定性随机序列。

`blake3::Hasher::update_with_seek(seed, player_id * 256 + counter)` 一行代码替代整个 ChaCha keystream 管理。

---

## 9. 证书: Ed25519

### 备选

| 方案 | 签名大小 | 签名速度 | 验证速度 | 选择理由 |
|------|---------|---------|---------|---------|
| Ed25519 | 64 B | ~70k/s | ~30k/s | **选择** — 广泛标准，纯 Rust 实现好 |
| ECDSA P-256 | 64 B | ~30k/s | ~15k/s | 慢，NIST 曲线有信任问题 |
| RSA-2048 | 256 B | ~1k/s | ~50k/s | 签名太大，慢 |
| secp256k1 | 64 B | ~40k/s | ~15k/s | 好但 Ed25519 在认证场景更标准 |

### 选择: Ed25519

证书签发是低频操作（24h 一次），但验证是高频（每次部署都验证）。Ed25519 的验证速度 ~30k/s，小签名 64B，纯 Rust `ed25519-dalek` 实现成熟。短期证书（24h）+ 服务端签发 + 吊销列表，形成完整的信任链。

---

## 10. SDK: TypeScript + Rust

### 备选

| 方案 | 类型 | 优势 | 劣势 |
|------|------|------|------|
| TypeScript | AI 玩家第一语言 | AI agent 生态（MCP SDK、LLM 工具链）；Web 同构 | 性能上限 |
| Rust | 人类硬核玩家 | 性能顶尖；类型安全 | 上手门槛高 |
| Python | 科研/AI | 最多 AI 开发者会用 | 运行时太重；不适合 WASM |
| Go | 服务端 | 简单 | 编译到 WASM 受限（GC） |
| C/C++ | 底层 | 作为 WASM 底层目标 | 不需要专门 SDK |

### 选择: TypeScript + Rust

双 SDK 策略覆盖两类玩家：(1) AI agent 开发者——TypeScript 是 LLM 工具链的母语，AI 生成的代码大概率是 TS；(2) 追求性能的人类玩家——Rust 编译到 WASM 的路径在 Wasmtime 中是最高效的。两者都走 `game_api.idl → codegen → SDK` 的自动化路径，API 一致性由 IDL 保证。

---

## 11. Web UI: Monaco + PixiJS

### 备选

**编辑器**:

| 方案 | 优势 | 劣势 |
|------|------|------|
| Monaco | VS Code 内核；TypeScript 原生支持 | 相对重 (~5MB) |
| CodeMirror 6 | 更轻；模块化 | TypeScript 支持不如 Monaco |
| Ace | 成熟 | 不如 Monaco |

**渲染**:

| 方案 | 优势 | 劣势 |
|------|------|------|
| PixiJS | 最快的 2D WebGL；tilemap 原生 | WebGPU 还在迁移 |
| Phaser | 游戏框架全套 | 太重；2D 渲染不如 PixiJS 裸 |
| Three.js | 3D | Swarm 是 2D 游戏 |
| Canvas 2D | 零依赖 | 性能跟不上 500 drone |

### 选择: Monaco + PixiJS

Monaco 的 TypeScript 智能提示直接对接 SDK 类型——玩家写 `drone.` 弹出 `harvest/move/transfer`。PixiJS 的 tilemap 渲染 `MAX_QUERY_RANGE` 内的可见实体，WebGL 加速下 500 drone 不卡。两者都是各自领域的第一梯队，且彼此无冲突。
