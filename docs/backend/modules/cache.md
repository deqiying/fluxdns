# Cache 模块设计

> 文档状态：有效
>
> 实现状态：部分实现
>
> 适用范围：缓存 key、TTL、single-flight、memory adapter 和 persistence 生命周期
>
> 最后核对：2026-09-04
>
> 关联实现：`backend/src/cache/*`
>
> 关联文档：[后端架构](../architecture.md) · [配置字段参考](../configuration-reference.md) · [Ports](ports.md) · [DNS Core](dns-core.md) · [Storage](storage.md)

## 当前实现边界

v1 方案已完成，已实现内存 CacheStore、Moka CacheStore、文件快照和 SQLite cache persistence 首轮 adapter、文件/SQLite 基础 contract、SQLite metadata/disk-usage 观测和 test-only 故障重试 contract、有界 persistence writer 生命周期、production async recovery/write/shutdown 接线与停机摘要、一致的共享容量淘汰、响应准入/TTL、稳定 key builder、client identity 摘要隔离、CacheFacade 首轮切片、fresh 剩余 TTL/stale answer TTL、可取消有界 LateCacheFinalizer、RuntimeCoordinator 级历史/当前 owner、PolicyDnsCore 当前 snapshot-local optimistic refresh 边界。跨 adapter 的真实 disk-full 故障测试矩阵尚未完成。

## 1. 职责

Cache 模块实现逻辑缓存池、entry 生命周期、single-flight、optimistic refresh、内存存储和独立持久化存储。

内部结构：

| 文件 | 职责 |
| --- | --- |
| `key.rs` | namespace、query/strategy/ECS/transport compatibility key 的稳定编码 |
| `memory.rs` | 无外部依赖的 HashMap/Mutex `CacheStore` adapter，提供确定性语义基线和 single-flight 实现 |
| `moka.rs` | 基于 `moka::sync::Cache` 的并发 `CacheStore` adapter，复用同一 port 语义和 single-flight 边界 |
| `persistence.rs` | 无外部依赖的版本化文件快照 `PersistentCacheStore` adapter，作为 SQLite adapter 的 codec 基线 |
| `sqlite.rs` | 独立 SQLite cache persistence adapter，使用 WAL、独立 schema 和批量事务写入 |
| `runtime.rs` | 有界非阻塞 persistence writer、单写者任务和有序 shutdown |
| `service.rs` | `CacheFacade`、single-flight、TTL、CAS、invalidations 和 `LateCacheFinalizer` 的 typed 编排 |

缓存 SQLite 与业务统计 SQLite 是不同文件、不同 schema、不同 writer 和不同故障边界。

## 2. Namespace

每个请求最多选择一个：

- `global`；
- `strategy:<strategy-id>`；
- `client:<identity-digest>:strategy:<strategy-id>`；
- disabled。

namespace 使用稳定 typed components 编码，不直接拼接可伪造原始 client ID。持久化格式包含 namespace format version。

## 3. Cache key

key 至少包含：

- namespace；
- canonical QNAME/QTYPE/QCLASS；
- 会改变答案的 DNS flags；
- effective ECS；
- strategy/route 产生的 target identity；
- opaque transport compatibility；
- cache key format version。

不包含：

- 客户端 DNS ID；
- RuntimeSnapshot revision；
- 全局 resource generation；
- HTTP header 或 URL；
- 原始 client address。

资源变更不会因 key generation 改变而清空全局缓存。

## 4. Entry

entry 包含：

- canonical response；
- inserted/expiry/stale-until 时间；
- 原始 RR TTL metadata；
- response class；
- producer runtime revision；
- 相关 resource fingerprints；
- quality rank；
- checksum 和 format version；
- 缓存生产请求的 upstream target 与实际 direct/member provenance；
- 上游 TC 时的 transport compatibility。

resource fingerprint 用于诊断和显式清理，不是普通 lookup 的自动失效条件。

## 5. TTL 与准入

- 正常 NOERROR：按可用 RR TTL 计算 entry 生命周期，返回时逐 RR 扣减；
- NODATA/NXDOMAIN：优先使用 SOA TTL 与 MINIMUM 的较小值，无可用值时使用 `failure_ttl`；
- SERVFAIL 和上游 TC：使用 `failure_ttl`；
- REFUSED：不缓存；
- malformed、question mismatch、连接/TLS/HTTP failure、timeout：不缓存。

TTL override 只影响 client-visible TTL，不延长 entry expiry。

group member ECS 在成员选择后才确定，不能安全复用仅含 group ID 的 cache key；当前实现对存在显式 member ECS 且未被 rule/strategy/client 覆盖的 group 绕过 lookup、single-flight 和写入，待未来 key 契约能表达成员选择后再开放。

quality 从高到低：

1. 完整 NOERROR/TC=0；
2. NODATA/NXDOMAIN；
3. SERVFAIL/TC。

高质量可替换低质量；低质量不能覆盖仍 fresh 的高质量。同质量默认保留先到值到 expiry，避免竞态抖动。

## 6. Lookup 状态

`CacheFacade` 返回：

- `Disabled`；
- `Miss`；
- `Fresh(entry)`；
- `Stale(entry, refresh-permit)`；
- `StoreUnavailable`。

store unavailable 时继续上游解析；已有可安全读取的内存 entry 仍可返回。

## 7. Single-flight

single-flight key 与 cache key 相同：

- 首个 miss 创建 producer；
- 后续 waiter 订阅 producer 结果；
- 单 waiter 取消只移除自身；
- 无 waiter 且无 late cache value 时可以取消 producer；
- optimistic refresh 即使没有当前 waiter，也在 refresh deadline 内保留 producer；
- producer 完成后以 CAS 写入并广播；
- panic/取消必须释放占位，不能永久阻塞 key。

为防止高基数攻击，占位表使用容量和空闲超时上限；超过上限时允许请求独立解析并计数，而不是全局阻塞。

## 8. Optimistic refresh

stale entry 仅在：

- optimistic enabled；
- 当前时间不超过 stale-until；
- entry 仍满足 transport compatibility；
- response class 允许 stale；
- refresh admission 有容量

时先返回。

完整设计中的 refresh 应捕获启动时最新 RuntimeSnapshot，完整重跑 client/policy/resource/upstream 流程，不能复用旧 entry 保存的 connector 或 rule pointer。当前实现先限定在已构建的 immutable `PolicyDnsCore`：stale 响应立即返回，按一次性 refresh permit 通过有界 `LateCacheFinalizer::submit_task` 在独立 deadline/cancellation 下重新执行当前 upstream，并以 `CacheCondition::Version` 尝试写回；Runtime 最新 snapshot 捕获仍待后续生命周期接线。

写回以 key + quality + producer revision 做 CAS；旧 producer 不覆盖新完整答案。

## 9. Memory store

目标 Moka store：

- 全部 namespace 共享一个容量预算；
- weigher 计入 key、canonical wire、TTL metadata 和索引开销；
- 计费值用于 eviction，不宣称等于 RSS；
- per-entry expiry；
- eviction listener 只发送低成本统计，不执行阻塞 I/O。

Moka adapter 不向 DNS Core 暴露具体 entry guard 或 future 类型。

当前已实现的首轮内存 adapter 使用 `HashMap + Mutex` 保持无外部依赖和确定性：

- lookup 区分 fresh、stale 和已过期 entry；已过期且超出 stale 窗口的 entry 会被移除；
- CAS 检查 `Absent/Version` 条件，并拒绝较低质量覆盖仍 fresh 的高质量 entry；
- 支持 exact、namespace、typed predicate 和 all 失效；
- single-flight 保证每个 key 只有一个 leader，follower 可独立取消，leader abandon/drop 会广播稳定失败；
- shutdown 会清理记录、唤醒 waiter，并拒绝后续读写；
- `weighted_size` 仅作为内部计费统计，不宣称等于进程 RSS；当前已支持共享 weight 上限、确定性 oldest eviction、oversized entry 拒绝和 eviction 计数。

Moka adapter 复用上述 `CacheStore` contract，但把实际 entry 存储和容量淘汰交给 `moka::sync::Cache`：

- 所有 namespace 共享同一个 `max_capacity` weight 预算；weigher 计入稳定 key、canonical wire 和固定 entry 开销，过大的 entry 在 CAS 前返回 `resource-exhausted`；
- `Expiry` 以 `expires_at` 和 `stale_until` 中较晚者作为物理移除边界，因此 stale entry 仍可被 `CacheFacade` 观察，超过 stale 窗口后才由 adapter 移除；
- eviction listener 只统计 Moka 的 size eviction，不把显式失效、替换或 TTL 过期误计入容量淘汰；
- single-flight reservation/wait/publish/abandon 复用已验证的 typed 实现，Moka 只负责 record 存储，不向 DNS Core 暴露 Moka 类型。

`PolicyDnsCore` 的默认构造路径已使用 `MokaCacheStore::with_max_weight`；测试和未来替代实现仍可通过 `CacheStore` trait 注入其他 adapter。

当前已实现的纯逻辑准入 helper：

- 将 `CanonicalResponse` 映射为 `NoError/NoData/NxDomain/ServFail/Truncated` cache class 和质量等级；
- 正常响应使用 origin TTL，负响应优先使用 SOA 负 TTL，无值时回退 `failure_ttl`；SERVFAIL/TC 使用 `failure_ttl`；
- REFUSED、未知响应类、缺失 TTL 和零 TTL 明确拒绝写入；
- 可选生成 optimistic stale 窗口，并为 canonical wire 计算稳定 checksum。

当前已实现的 key/facade/finalizer 首轮切片：

- `CacheKey` 使用固定 format version、长度前缀和 opaque namespace/compatibility 组件编码 canonical query，不包含 DNS ID、runtime revision 或原始 client 地址；
- `CacheFacade` 将 disabled、miss、fresh、stale 和 store unavailable 分层，并用一次性 refresh permit 防止重复刷新许可；共享 facade 在任一 global/strategy/client 逻辑池开启时启用，`dns.cache.enabled` 只控制全局池；client pool 按实际命中的 ID/IP 生成域分隔 SHA-256 摘要，缓存键不保存原始身份；
- write request 经过准入 helper 后再进入 typed CAS，adapter 错误保持为可降级的 store unavailable，不把具体存储类型泄漏到 DNS Core。
- `LateCacheFinalizer` 使用有界 semaphore 接收客户端响应完成后的 typed write request；`submit_task` 也可承载 optimistic refresh 等后台任务。提交容量不足时拒绝，shutdown 取消并等待已提交任务退出，不影响客户端 response。
- `PolicyDnsCore` 返回 fresh response 前按已缓存整秒递减 RR TTL；共享 store 以所有启用 pool 的最大 optimistic `max_age` 保留候选，返回 stale response 前再按当前所选 pool 的 `max_age` 限制并应用其 `answer_ttl`；之后通过一次性 refresh permit 和当前 immutable core 的 upstream exchange、`CacheCondition::Version` 完成后台写回。后台 exchange、question mismatch 或 CAS/store 失败只放弃刷新，不改变已返回的客户端响应。

## 10. Persistence

当前已实现的首轮 persistence 边界使用 `FDCP` 版本化二进制快照文件：

- `FilePersistentCacheStore` 通过 `PersistentCacheStore` port 暴露恢复、写入、容量维护和 shutdown；
- 快照写入使用唯一临时文件、`sync_all` 和 `rename`，不在 DNS 请求路径执行；
- 恢复验证 magic、format、record/payload/wire 大小、canonical response、checksum、response class 和过期时间；
- entry format v2 在 payload 中保存已验证的 upstream target/used ID；文件快照中的 v1 entry 按 incompatible 隔离；
- expired、corrupt、incompatible record 在 record 级隔离并计数；文件超出预算或整体 framing 损坏时拒绝恢复；
- 超出 page budget 时按 inserted-at、version 和稳定 key 顺序淘汰最旧 entry。

该 adapter 固定持久化 port 和恢复故障语义，不等价于完整 v1 目标中的 SQLite cache。

当前已实现的 `SqlitePersistentCacheStore` 首轮 adapter：

- 使用独立 `SqlitePool`、独立 `cache_entries` schema 和 WAL/`synchronous=NORMAL` 初始化，不复用业务 Storage pool 或表；
- `persist` 在单事务中合并已存在记录、复用 FDCP codec 做 canonical/checksum/expiry 校验、按 cache max-size budget 淘汰后重写 payload rows；
- 已知 `schema=1/cache_format=1/key_format=1` 会事务化清空可再生旧 cache entry 并升级为 format 2；其他未知 metadata 组合继续拒绝打开；
- `recover` 在 adapter 边界隔离过期、损坏和不兼容记录并返回 `CacheRecoverySummary`；
- `recover`、`persist`、`maintain_capacity` 与 `shutdown` 复用同一串行 operation lock，完整数据库 future 和锁等待均受调用方 deadline 约束；数据库关闭后拒绝继续恢复/写入。

SQLite `cache_meta` 版本契约和主库/WAL/SHM `disk_usage()` 观测 API 已实现；真实 SQLite Busy 已验证旧 snapshot 保留和后续重试，last-access bucket、真实 disk-full recovery 和剩余跨 adapter 故障 contract 仍待后续阶段实现。

阶段 125 增加 `CachePersistenceRuntime`：DNS 侧通过有界 `try_send` 提交 typed batch，单写者任务串行执行 adapter I/O，单批失败按 best-effort 计数后继续，正常 shutdown 按 FIFO 排空已入队批次并关闭 adapter。

阶段 126 将该 runtime 接入 production async prepare：任一逻辑缓存池启用时打开独立 SQLite，将恢复的可用 entry 写入 Moka；内存 CAS 成功后仅做 non-blocking enqueue。现有 `LateCacheFinalizer` owner 同时托管 persistence，并由 `RuntimeCoordinator` 在 service shutdown deadline 内排空关闭。连接或恢复失败记录 degraded warning 后保留纯内存 cache，不阻止 DNS 启动；同步构造器继续无磁盘副作用。

阶段 152 将各 finalizer owner 的 `CachePersistenceRunSummary` 汇总到 service shutdown：成功、失败、队列丢弃和容量清理均以安全计数输出；未按 deadline 关闭或出现失败/丢弃批次时，在 Telemetry 关闭前发布 Cache degraded health 与 persistence gap。该观测不把缓存 key、响应内容或 adapter 原始错误写入日志，也不把 best-effort persistence 失败升级为 DNS 请求失败。

阶段 97 已补充文件快照与 SQLite adapter 的基础 contract test，锁定 live/expired recovery、记录字段、容量维护和 shutdown 后拒绝操作的一致语义；真实数据库 disk-full 故障矩阵仍未实现。

阶段 98 为 SQLite adapter 增加 `cfg(test)` 一次性 Busy/DiskFull 注入；失败写入返回 `Unavailable` 且不改变已持久化记录，清除注入后下一次写入成功。该 hook 只用于 deterministic retry 验证，不等价于真实 OS/SQLite 故障复现。

阶段 99 统一 `FilePersistentCacheStore` 与 `SqlitePersistentCacheStore` 的 `maintain_capacity` 语义：过期、损坏和不兼容记录会计入移除数，并在维护后从持久化介质清除；跨 adapter contract 已覆盖过期清理后的恢复结果。

阶段 100 在 SQLite cache DB 中落地 `cache_meta`，记录并校验 schema、cache record 和 cache key format version；重开时版本不匹配会拒绝 adapter，避免用错误 codec 读取旧数据。

阶段 101 增加 `SqliteCacheDiskUsage`，以统一 API 返回主库、WAL、SHM 和总字节数；不存在的 sidecar 按 0 计，shutdown 后观测请求按 unavailable 处理。

独立 SQLite cache DB 至少包含：

- metadata：schema/cache format/key format；
- cache entry；
- namespace/key 索引；
- checksum、expiry 和 last-access bucket。

写入经有界队列批量提交。超过 page budget 时优先删除 expired，再按近似 LRU/last-access bucket 淘汰。WAL/SHM 短时空间不计入主文件预算，但必须可观测。

启动恢复：

1. 检查 schema/format；
2. 验证 checksum；
3. 丢弃 expired/corrupt/incompatible entry；
4. 分批注入 memory store；
5. 任一步失败只禁用 persistence 并标记 degraded，不阻止 DNS 启动。

## 11. 显式失效

提供：

- exact key；
- namespace；
- typed predicate；
- 全部缓存。

普通 resource refresh 不调用这些接口。未来 WebUI 清除缓存必须通过同一 facade 并产生审计事件。

## 12. 故障语义

- memory store 内部错误：绕过缓存继续解析；
- persistence queue 满/DB busy/disk full：停止或丢弃持久化写，保留内存；
- recovery corrupt entry：隔离并计数，不加载；
- single-flight producer panic：唤醒 waiter 为 failure，清理占位；
- stale refresh failure：保留原 entry 到 stale-until，不延长窗口。

## 13. 测试

- namespace 选择和 key 稳定性；
- 正向/负向/failure TTL；
- REFUSED 和 transport failure 不准入；
- quality CAS 和并发乱序；
- single-flight 多 waiter 与取消；
- [x] optimistic 使用最新 Runtime snapshot；`RuntimeCoordinator` current-target cell 已让旧 Runtime 的 refresh/late sink 路由到最新 cache/finalizer，目标缺失时回退 snapshot-local 语义
- resource update 不全局失效；
- Moka weight/expiry；
- persistence format/checksum/recovery/page budget；
- DB busy/disk full 降级不影响 DNS；
- explicit invalidation 范围准确。

## 14. 实现检查清单

- [x] 定义 namespace/key/entry format；（基础 typed contract 与 `cache/key.rs` 稳定编码已完成）
- [x] 实现 CacheFacade 首轮切片；（基础 DNS Core fresh/miss/single-flight/CAS 接线已完成）
- [x] 实现 namespace/key builder；
- [x] 实现 single-flight/CAS/显式失效的内存 adapter 首轮切片；
- [x] 实现共享容量淘汰和 oversized entry 边界；
- [x] 实现可替换 `PersistentCacheStore` port 的文件快照 adapter；
- [x] 实现可取消、有界的 `LateCacheFinalizer`；（当前已接入 PolicyDnsCore snapshot-local optimistic refresh，parallel 快速完整 Positive late sink 已消费；RuntimeCoordinator 已统一托管历史/当前 owner，完整 late-window/nested sink 传播仍待完整 Cache-Core 管线）
- [x] 实现 Moka adapter；
- [x] 实现独立 SQLite persistence 首轮 adapter；
- [x] 在 SQLite cache DB 中记录并校验 schema/cache/key format metadata；
- [x] 在 `CacheEntry` 中保存 producer upstream provenance，并贯通 miss、single-flight、fresh/stale、optimistic refresh、late result 和文件/SQLite persistence；
- [x] 提供 SQLite 主库/WAL/SHM 磁盘占用观测 API；
- [x] 实现有界 persistence writer 和有序 shutdown 生命周期；
- [x] 验证 persistence adapter 停机阻塞超过 deadline 时 abort worker 并返回稳定 `Timeout`；
- [x] 将 SQLite recovery、CacheFacade non-blocking write 和 Runtime shutdown 接入 production async prepare；
- [x] 汇总历史/当前 Runtime 的 persistence 停机计数，并在 Telemetry 关闭前发布安全 gap 状态；
- [x] 完成内存 adapter 的 fresh/stale/expiry、质量 CAS、失效、取消、abandon 和 shutdown 测试；
- [x] 完成文件/SQLite adapter 的基础一致性、恢复和 shutdown contract 测试。
- [x] 完成 SQLite adapter test-only Busy/DiskFull 失败后重试 contract 测试。
- [x] 完成 SQLite adapter 的真实 Busy 写锁、旧 snapshot 保留和重试测试。
- [x] 完成 SQLite adapter 串行 operation lock 的 deadline 与 shutdown 超时测试。
- [x] 完成真实 SQLite Busy 下调用方短 deadline 优先返回 `Timeout` 的测试。
- [x] 以 Memory 与 Moka adapter 共用测试验证热路径 `CacheStore` 契约。
- [ ] 完成跨 adapter 的真实 disk-full 故障测试矩阵。

阶段证据：内存/cache focused tests 覆盖 fresh/stale/expiry、质量 CAS、失效、single-flight cancellation/abandon、shutdown、响应分类、TTL、stale 窗口、checksum、稳定 key、Facade 状态、容量淘汰和 `LateCacheFinalizer` 的异步写入/取消；`cache::runtime::tests` 覆盖参数拒绝、失败批次隔离、摘要合并、shutdown 排空和 adapter 停机超时 abort，`cache::service::tests` 覆盖 memory CAS 后的 writer 接线；PolicyCore 定向测试覆盖配置启用缓存后的 upstream 命中、跨 core SQLite 恢复、fresh 剩余 TTL、stale answer TTL、snapshot-local optimistic refresh、fast-positive late sink 写入、最终 ECS subnet/client identity 的 cache key 隔离，以及成员 ECS group 的安全缓存绕过；async prepare 定向测试覆盖 bind 前 persistence owner 构造及有序 shutdown；Runtime coordinator/service 定向测试覆盖 previous/current Runtime finalizer owner 的摘要合并、统一 shutdown 回收和 Cache health 发布；upstream executor 覆盖 nested parallel group sink 传播；文件快照、Moka、SQLite 和跨 adapter contract 覆盖 roundtrip、expiry、容量、checksum、metadata、disk usage、shutdown 及 test-only Busy/DiskFull 重试。阶段 190 后端全量 `555 passed、0 failed`；阶段 184 以独立连接持有真实 SQLite 写锁，验证 Busy 返回 `Unavailable`、旧 snapshot 不变且释放锁后重试成功；阶段 186 使 SQLite cache 的串行锁等待遵守 deadline；阶段 188 进一步限制完整数据库 future，真实 Busy 下短 deadline 优先返回 `Timeout`；阶段 195 以共享测试验证 Memory 与 Moka `CacheStore` 契约，`2 passed、0 failed`。last-access writer 和真实 disk-full 故障矩阵仍未完成。

当前实现进度：**87%**（已完成内存/Moka/SQLite 首轮 adapter、Cache-Core 主链、稳定 key/TTL/CAS、latest-target finalizer、production recovery/non-blocking persistence、历史/当前 owner 有序 shutdown、完整 SQLite future deadline、安全 gap 摘要与真实 SQLite Busy 重试；完整 late-window 候选生命周期、last-access bucket、真实 disk-full 数据库故障恢复与跨 adapter 真实故障矩阵未实现）。
