# Cache 模块设计

> 文档状态：有效
>
> 适用范围：缓存 key、TTL、single-flight、memory adapter 和 snapshot 生命周期
>
> 最后评审：2026-09-08（内存权威与独立完整快照；其余基线见[模块索引](README.md)，实际接线见[后台服务](../../../implementation/backend/background-services.md#cache-persistence)）
>
> 关联实现：[service.rs](../../../../backend/src/cache/service.rs)、[moka.rs](../../../../backend/src/cache/moka.rs)、[snapshot.rs](../../../../backend/src/cache/snapshot.rs)、[persistence.rs](../../../../backend/src/cache/persistence.rs)
>
> 关联文档：[后端设计](../overview.md) · [配置参考](../../../implementation/configuration.md) · [DNS 管线](../../../implementation/backend/dns-pipeline.md) · [后台服务](../../../implementation/backend/background-services.md)

## 1. 职责与边界

Cache 管理逻辑池、entry 生命周期、single-flight、optimistic refresh 和独立完整快照。`CacheFacade` 编排语义，Moka 内存集合是缓存唯一权威；快照只负责进程启动预热和周期覆盖，不维护第二份增量集合。缓存文件与业务统计 SQLite、详情日分片使用不同路径、owner 和故障边界。

| 组件 | 设计职责 |
| --- | --- |
| key / admission | 稳定编码、响应分类、TTL、checksum 与质量 |
| memory / Moka adapter | 同一 CacheStore 契约、容量和逐 entry 过期 |
| snapshot codec / file adapter | 完整文件头、流式记录、完整性校验、恢复与原子发布 |
| snapshot owner | 进程级周期覆盖、generation 仲裁、冷启降级和有序关闭 |
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

refresh 应捕获启动时最新可用 RuntimeSnapshot，完整重跑 client/policy/resource/upstream，不复用 entry 中的旧 connector/rule pointer。写回按 key、quality 和 producer revision CAS，旧 producer 不能覆盖新完整答案。资源更新与跨 revision finalizer 的实际接线见[DNS 管线](../../../implementation/backend/dns-pipeline.md)；组合证据见[Late-window 与 owner](../../../implementation/backend/dns-pipeline.md#late-window-与-owner)，不以设计句子宣称全部组合验收完成。

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

## 7. Snapshot

快照使用单个非 SQLite 二进制文件。文件头包含独立 snapshot format version、生成时间、记录数、body 长度和 SHA-256 完整性摘要；每条记录继续复用 cache key/entry format、canonical response、绝对到期时间、fingerprint 及 upstream provenance。快照格式版本不复用配置、详情 layout 或 WS 版本。

一个进程级 owner 从 Moka 当前可见集合分批取得记录，释放查询锁后编码并顺序写入同目录临时文件。单条和总记录数有内部上界；同一轮按完整 key 摘要去重。并发更新、淘汰可能让本轮少量记录未被捕获，这是允许的弱一致性，不得通过保留旧磁盘条目补齐。完成 header、长度和摘要后 flush/sync，再经过 owner generation 仲裁发布；失败保留上一份完整文件。

快照没有用户可配置的磁盘大小配额，也不承诺文件大小等于 Moka weight 或 RSS。读取仍受文件字节数、单条大小、记录数、批次和 deadline 保护；先验证整个文件的长度与摘要，再分批解码。恢复逐条检查 key/entry version、checksum、绝对 expiry/stale-until 和当前内存预算，停机时间不会重新补满 TTL。损坏、未知版本、超时或预算不足只形成冷启/部分恢复状态，不阻止 DNS 服务。

内存 commit 不再产生逐条 persistence 队列。周期任务覆盖当时的完整可见集合，被内存预算淘汰或显式清理的记录会从下一份快照消失。owner/path 切换和未来 clear 必须递增 generation，使旧任务失去发布权；正常 shutdown 只在统一剩余预算内尽力补写，不无限延长退出。

当前生产仍使用 SQLite 增量 persistence，直到活动计划 BC-07 完成 owner、启动恢复、reload/shutdown 接线并退出该旧路径；BC-06 的 codec/真实文件能力不能单独作为生产切换证据。

## 8. 显式失效

Facade 提供 exact key、namespace、typed predicate 和 all 失效。普通资源刷新不调用这些接口。WebUI 当前没有缓存清除功能；将来增加时必须先评审权限/审计，不直接操作 store。

## 9. 契约验证要求

- namespace、Fast/Resolved 不 alias、fingerprint include/exclude 与资源更新不全局 clear。
- 正/负/failure TTL、REFUSED 拒绝、质量 CAS、并发乱序和 client-visible TTL 隔离。
- 多 waiter 取消、candidate drop、commit 终态、占位上限和关闭后拒绝。
- optimistic 最新资源/目标、跨 runtime late-window、独立 deadline 和失败不延长 stale。
- Moka weight/expiry、分批导出、format/header/checksum/recovery、内存预算与实际文件大小的区别。
- 真实替换/权限/空间失败与恢复不破坏上一份快照、不阻塞 DNS；测试 hook 不替代真实介质。
- 显式失效范围、历史 owner drain、失败摘要和秘密不进入日志。

这些是验证要求，不是本次通过记录。当前构造与证据见[后台服务实现](../../../implementation/backend/background-services.md)。
