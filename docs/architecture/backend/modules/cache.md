# Cache 模块设计

> 文档状态：有效
>
> 适用范围：缓存 key、TTL、single-flight、memory adapter 和 persistence 生命周期
>
> 最后评审：2026-09-05（模块边界与关键契约静态核对，基线见[模块索引](README.md)；不含运行验收）
>
> 关联实现：[service.rs](../../../../backend/src/cache/service.rs)、[moka.rs](../../../../backend/src/cache/moka.rs)、[sqlite.rs](../../../../backend/src/cache/sqlite.rs)、[persistence.rs](../../../../backend/src/cache/persistence.rs)
>
> 关联文档：[后端设计](../overview.md) · [配置参考](../../../implementation/configuration.md) · [DNS 管线](../../../implementation/backend/dns-pipeline.md) · [后台服务](../../../implementation/backend/background-services.md)

## 1. 职责与边界

Cache 管理逻辑池、entry 生命周期、single-flight、optimistic refresh 和独立持久化。`CacheFacade` 编排语义，`CacheStore` 与 `PersistentCacheStore` 隔离具体存储。缓存 SQLite 与业务统计 SQLite 是不同文件、schema、writer 和故障边界。

| 组件 | 设计职责 |
| --- | --- |
| key / admission | 稳定编码、响应分类、TTL、checksum 与质量 |
| memory / Moka adapter | 同一 CacheStore 契约、容量和逐 entry 过期 |
| persistence codec / SQLite adapter | format 校验、批量事务、恢复与容量维护 |
| persistence runtime | 有界非阻塞入队、单写者和有序关闭 |
| facade / commit candidate / finalizer | lookup、CAS、single-flight lease 和晚到结果生命周期 |

## 2. Namespace 与 key

每个请求最多选择 global、strategy、client-identity+strategy 或 disabled 中的一个逻辑池，不逐层查询 fallback。具体继承只由[配置参考](../../../implementation/configuration.md)定义。

namespace 使用稳定 typed components，不拼接可伪造原始 client ID。client identity 使用域分隔 digest，持久化格式包含版本。

key format v2 包含 namespace、canonical query wire、opaque transport compatibility、Fast/Resolved mode byte、可选的 policy/request/target/ECS 32-byte fingerprint 与版本。不得包含客户端 DNS ID、整个 runtime revision、全局 resource generation、HTTP header/URL 或原始 client address。

Fast 在逐规则 matcher 前构造，fingerprint 覆盖策略语义与当前 PolicyState 中全部 hosts/rule-set content hash；Resolved 在完整决策后加入 target/final ECS。两种编码不能 alias。资源变化切换 key 而不主动扫描清空全库；旧项继续占容量到自然过期/淘汰。当前失效粒度可能大于实际依赖资源集合，见 [Policy](policy.md)。

group member ECS 在选择后才确定且无上层覆盖时，必须绕过不安全的 lookup、single-flight 和写入，不能仅按 group ID 复用不同成员答案。

## 3. Entry、TTL 与质量

entry 保存 canonical response、inserted/expiry/stale-until、原始 RR TTL、response class、producer revision、quality、checksum/format，以及缓存生产请求的 target/actual upstream provenance。上游 TC 额外受 transport compatibility 限制。policy/resource fingerprint 已在 key 内；producer revision 只用于诊断/CAS，不作为全局失效开关。

- 正常 NOERROR 按可用 RR TTL 决定生命周期，返回时逐 RR 扣减。
- NODATA/NXDOMAIN 优先取 SOA TTL 与 MINIMUM 较小值，无可用值时用 failure TTL。
- SERVFAIL/上游 TC 使用 failure TTL；REFUSED、未知类、零/缺失 TTL、malformed、question mismatch、连接/TLS/HTTP failure 或 timeout 不准入。
- TTL override 只改变 client-visible TTL，不延长 entry expiry；持有 origin response 的候选不得被输出覆写污染。
- 质量顺序是完整 NOERROR/TC=0、NODATA/NXDOMAIN、SERVFAIL/TC；低质量不得覆盖仍 fresh 的高质量，同质量默认保留先到值到 expiry。

## 4. Lookup 与 single-flight

Facade 区分 Disabled、Miss、Fresh、Stale+一次性 refresh permit、StoreUnavailable。store unavailable 时继续解析，不能把存储错误伪装成正常命中。

single-flight key 与 cache key 一致：

1. 首个 miss 创建 producer，后续 waiter 订阅同一结果；单 waiter 取消不影响其他人。
2. 无 waiter 且无 late cache value 时可取消 producer；optimistic refresh 在独立窗口内可以继续。
3. producer 返回共享 response，将请求、origin response 和不可 clone 的 RAII lease 移交 `CacheCommitCandidate`。
4. 后台 cache worker 使用独立 100ms deadline 执行 admission/CAS/persistence enqueue，发布 Ready/Miss/Failed。
5. 队列拒绝、取消、panic、abandon 或 drop 都必须结束 lease 并唤醒 waiter，不能永久占位。

占位表受容量和空闲超时保护；超限允许独立解析并计数，不全局阻塞。异步 Stored/Rejected/Conflict/Unavailable/Dropped 与响应前的 cache lookup status 分别计数。

## 5. Optimistic 与 late result

只有 optimistic 开启、未超过 stale-until、transport compatible、响应类允许且 refresh admission 有容量时才可先返回 stale。共享 store 可按启用池中最大 max_age 保留候选，实际返回仍按当前所选池的 max_age 与 answer TTL 限制，再应用输出 TTL override。

refresh 应捕获启动时最新可用 RuntimeSnapshot，完整重跑 client/policy/resource/upstream，不复用 entry 中的旧 connector/rule pointer。写回按 key、quality 和 producer revision CAS，旧 producer 不能覆盖新完整答案。资源更新与跨 revision finalizer 的实际接线见[DNS 管线](../../../implementation/backend/dns-pipeline.md)；完整 late-window 场景仍由[差距计划](../../../plans/backend-contract-gaps.md)跟踪，不以设计句子宣称验收完成。

finalizer 以有界 semaphore 接收 typed write/refresh task，容量不足明确拒绝；shutdown 取消并等待已接收任务，晚到结果不改变已返回客户端的 response。exchange、question mismatch 或 CAS 失败只放弃刷新，不延长旧 entry 的 stale 窗口。

## 6. Memory store

生产选用 Moka，替代内存 adapter 必须遵循同一契约：

- 所有 namespace 共享一个 weight 预算；计入 key、wire、索引/元数据，不承诺等于 RSS。
- oversized entry 在 CAS 前明确拒绝，不能绕过全局预算。
- 物理 expiry 取 expires_at/stale_until 中较晚者，保证 Facade 能观察合法 stale。
- size eviction 单独计数，不混入显式失效、替换和 TTL 过期。
- single-flight reservation/wait/publish/abandon 与 record 存储解耦，不向 Core 暴露 Moka guard/future。
- shutdown 清理记录、唤醒 waiter，后续操作返回关闭状态。

确定性 HashMap/Mutex adapter 用于替代实现和契约测试，不是生产默认的证据。

## 7. Persistence

独立 SQLite cache DB 使用 `cache_meta` 保存 schema/cache/key format，`cache_entries(id, payload)` 保存 codec 编码记录。namespace、key、checksum、expiry 等在 payload 内，不是独立 SQL 列，也没有 namespace/key 数据库索引。恢复时经 codec 校验后构造内存 map，旧版本按显式兼容策略处理，不能混读。

写入通过有界队列提交给单 writer。SQLite `persist` 每批读取并解码现有记录、合并新批次、裁剪，再在事务内删除并重写保留集，不是按 key 增量 upsert。recover、persist、maintain_capacity 和 shutdown 共用 operation lock 和调用者 deadline；关闭后拒绝操作。

`max_size_bytes` 当前约束 `prepare_snapshot` 计算的编码快照字节数，不是 SQLite `max_page_count` 或实际文件硬上限。过期/损坏/不兼容记录在解码时过滤，超出编码预算再淘汰；数据库页、freelist、索引及 WAL/SHM 会额外占用空间。`disk_usage()` 可读取主库与 sidecar 大小，但不据此触发 page-budget checkpoint/收缩。物理容量要求的差距见[核对计划](../../../plans/backend-contract-gaps.md)。

容量裁剪按 entry 的 `inserted_at` 从旧到新淘汰，同值按 record version、encoded key 稳定排序。没有 last-access bucket，也不是按 SQLite row ID 或最近访问时间淘汰。既有近似 LRU 要求尚未实现，保留于[差距计划](../../../plans/backend-contract-gaps.md)评审。

启动恢复依次检查 schema/format、checksum、expiry/compatibility，再注入 memory store。无法恢复只禁用 persistence 并标记 degraded，不阻止 DNS 启动。内存 CAS 成功后的 enqueue 不等待磁盘；队列满、DB busy、disk full 均保持内存服务并记录 gap。

正常 shutdown 排空已入队批次并关闭 adapter；历史与当前 owner 共用总 deadline，成功/失败/drop/未完成摘要在 Telemetry 关闭前发布，不记录 key、response 或原始数据库错误。

## 8. 显式失效

Facade 提供 exact key、namespace、typed predicate 和 all 失效。普通资源刷新不调用这些接口。WebUI 当前没有缓存清除功能；将来增加时必须先评审权限/审计，不直接操作 store。

## 9. 契约验证要求

- namespace、Fast/Resolved 不 alias、fingerprint include/exclude 与资源更新不全局 clear。
- 正/负/failure TTL、REFUSED 拒绝、质量 CAS、并发乱序和 client-visible TTL 隔离。
- 多 waiter 取消、candidate drop、commit 终态、占位上限和关闭后拒绝。
- optimistic 最新资源/目标、跨 runtime late-window、独立 deadline 和失败不延长 stale。
- Moka weight/expiry、替代 adapter 一致性、format/checksum/recovery、编码快照预算与实际磁盘占用的区别。
- 真实 Busy/disk-full 与恢复不破坏旧数据、不阻塞 DNS；测试 hook 不替代真实介质。
- 显式失效范围、历史 owner drain、失败摘要和秘密不进入日志。

这些是验证要求，不是本次通过记录。当前构造与证据见[后台服务实现](../../../implementation/backend/background-services.md)。
