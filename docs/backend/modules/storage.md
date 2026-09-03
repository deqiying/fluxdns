# Storage 模块设计

> 文档状态：有效
>
> 实现状态：部分实现
>
> 适用范围：SQLite、统计、解析记录、migration、容量边界和存储生命周期
>
> 最后核对：待核对
>
> 关联实现：`backend/src/storage/*`、`backend/migrations/*`
>
> 关联文档：[后端架构](../architecture.md) · [配置字段参考](../configuration-reference.md) · [Ports](ports.md) · [Observability](observability.md) · [Cache](cache.md)

## 当前实现边界

v1 方案已完成，已实现纯内存统计 epoch/batch ledger、业务 schema v2 migration、SQLx SQLite storage 首轮 adapter、StatsPersistenceWorker、详情落库脱敏边界、group member、matched rule/resource、RCODE、failure 与 cancellation 摘要列、bounded detail writer channel、详情丢弃分类计数、限频 health 与成功恢复、年龄/软阈值/硬上限策略、worker shutdown drain/周期 flush、Storage stats/backend/detail 统一生命周期 facade、可共享 stats recorder、首轮 `StorageRuntime`/`DnsService`/`Supervisor` 生产接线、pending 内存保护/fatal 边界、首轮 degraded/recovery 状态转换、受 `cfg(test)` 限定的 Busy/DiskFull adapter fault 注入及恢复分类，以及 Policy Core 的低基数元数据传播。真实 OS disk-full 复现、migration 压力与故障测试和跨故障源 telemetry 闭环尚未完成。

## 1. 职责与边界

Storage 模块实现业务 SQLite：

- schema migration；
- 默认开启的聚合统计；
- 可选解析详情；
- writer 健康状态、flush 和 shutdown。

它不存储 DNS response cache。Cache persistence 使用配置中的独立文件和独立 `PersistentCacheStore`，不能复用本模块 pool、表或 writer。

内部结构：

| 文件 | 职责 |
| --- | --- |
| `sqlite.rs` | SQLx pool、PRAGMA、migration、统计/详情 transaction、bounded detail writer、详情淘汰/硬上限、worker shutdown drain/周期 flush、health/checkpoint/shutdown 首轮实现 |
| `service.rs` | `StorageRuntime` 组装、backend/detail flush 与 shutdown 顺序 facade |
| `stats.rs` | StatsAccumulator epoch snapshot、BatchLedger 顺序提交与失败重试 worker |
| `statistics.rs` | sharded counters、checkpoint、batch ledger |
| `resolve_log.rs` | 有界详情队列、批量写入和淘汰 |
| `writer.rs` | 无外部依赖的事务/幂等 writer contract 实现与 focused tests |

## 2. SQLite 初始化

prepare 阶段：

1. 创建数据库父目录；
2. 以读写/创建模式打开文件；
3. 设置 `foreign_keys=ON`、WAL 和有界 busy timeout；
4. 使用 `synchronous=NORMAL` 作为吞吐与崩溃恢复折中；
5. 运行嵌入 binary 的 SQLx migrations；
6. 执行小型写入/回滚探针；
7. 建立 stats/detail 独立 writer channel；
8. 返回 `StorageRuntime`。

数据库打不开、migration 失败、无写权限或基本写探针失败属于启动 fatal。

## 3. Schema 职责

具体 SQL 放入 migration，逻辑表至少包括：

### `storage_meta`

- schema version；
- instance/database ID；
- 创建与最近 migration 时间。

### `stats_daily_total`

- `day_utc`；
- `total_requests`；
- primary key 为 `day_utc`。

### `stats_daily_dimension`

- `day_utc`；
- `dimension_kind`；
- `dimension_value`；
- `count`；
- 复合 primary key。

`dimension_kind` 只允许 client bucket、transport class、strategy、source/upstream、RCODE、cache status 和有限 attempt outcome。`dimension_value` 必须来自配置/枚举 ID，不接受任意域名、完整 client ID 或原始 IP。

### `stats_batch_ledger`

- monotonic `batch_id`；
- `max_event_seq`；
- counter epoch；
- commit time；
- payload/hash 摘要。

用于幂等重试，不能与详情日志共享。

### `resolve_log`

- event time 与 request duration；
- request ID digest；
- listener、route、client bucket、strategy；
- canonical qname、qtype、qclass；
- 策略目标 upstream/group 与实际顶层 group member 的独立存在性标记；
- matched rule 来源、资源存在性标记和可选 ordinal；
- ECS mode/prefix length，不保存完整敏感 client ECS；
- source/upstream、RCODE、cache status；
- failure/cancellation 分类；
- runtime/resource revision 摘要。

解析详情本身是敏感数据；数据库文件使用工作目录权限保护，不把详情复制到服务日志。

## 4. 聚合统计热路径

请求线程只更新内存 sharded counters：

- 单请求分配 monotonic event sequence；
- `day_utc` 在事件发生时确定；
- 一次请求只增加一次 total；
- 维度 key 由 typed enum/ID 构造；
- parallel attempt 只进入 attempt counter，不重复 total。

更新不得 await、锁 SQLite 或向有界详情队列发送。

## 5. Stats checkpoint

writer 周期性执行：

1. 原子切换 counter epoch；
2. 冻结旧 epoch snapshot；
3. 分配 monotonic batch ID；
4. 在单事务中 upsert daily total/dimensions；
5. 写入 batch ledger；
6. commit 成功后 ack 并释放 snapshot；
7. commit 失败保留同一 batch 重试。

重试前先查 ledger；已提交 batch 不重复累加。新请求始终写下一 epoch，不等待旧批次。

当前 `StatsPersistenceWorker` 已实现上述首轮闭环：`record_request` 和 `StatsRecorder` 只触碰内存 accumulator，`flush` 先冻结 epoch，再将 pending batch 通过 `StorageBackend::execute` 按 batch ID 顺序提交；backend 失败时仅增加 batch 的失败尝试次数并保留原 payload，后续 flush 可继续幂等重试。worker 同时返回 committed batch/event 数量、pending 数量和 persistence gap 摘要。`StorageService` 普通 flush 按 stats → backend checkpoint → detail 执行，shutdown 按 stats → detail drain → backend close 执行，并可返回共享的同步 `StatsRecorder`。

进程在计数尚未进入 checkpoint 前崩溃会产生 in-memory persistence gap；系统必须报告，不承诺绝对无损。

## 6. Resolve log writer

详情开启时，DNS Core 通过 `ResolveEventSink` 尝试写入独立有界 channel：

- send 使用 non-blocking try-send；
- 队列满时丢弃当前详情并增加 `dropped_detail_records`；
- writer 按数量或时间批量提交；
- 提交失败有界重试，不阻塞 stats writer；
- `enable=false` 时 sink 为 no-op，但 stats 不受影响。

详情事件在进入队列前完成 redaction 和字段长度限制。

## 7. 淘汰和硬上限

每次详情 batch commit 在同一维护循环检查：

1. 删除早于 `max_record_age` 的记录；
2. 数量达到 `eviction_threshold_records` 时按时间/id 删除最旧记录；
3. 目标降到软阈值以下；
4. 插入前计算本事务后的数量；
5. 如果仍会超过 `max_records`，丢弃本批次中最晚到达的详情并计数。

硬上限判断和插入在同一 writer 串行路径中完成，避免并发突破。聚合表和 ledger 不受详情上限影响。

## 8. Connection 与事务

- stats 和 detail 使用同一业务数据库，但独立逻辑 writer；
- pool 保持小规模，避免 SQLite 写锁竞争；
- 两个 writer 的事务短且不互相 await；
- 只读查询未来通过独立 read connection；
- 所有 SQL 使用 bind 参数；
- migration 只在 prepare 执行。

## 9. 运行期故障

SQLite busy、磁盘满、I/O error：

- stats：保留未 ack batch，继续内存计数，进入 degraded；
- detail：允许丢弃并按 queue-full/policy 分类计数；queue-full 或 sink 错误在首次和 2 的幂次累计点发布 degraded/gap，下一条 accepted 恢复 Healthy；
- DNS 请求继续服务；
- 恢复后 stats 按 batch ledger 幂等补写；
- 记录 degraded 首发、最近重试、积压 batch、persistence gap 风险；
- pending batch/补偿计数达到 v1 固定内存保护上限时，stats 不能静默丢弃；升级为明确 fatal 或受控进程退出，由 supervisor 处理。

`StorageBackend` 自身 panic、schema corruption 或无法保证 ledger 正确性时升级 fatal，不继续写可能重复的统计。

周期 flush 和有序 shutdown 会把详情前端队列的 committed、pending、queue-full、sink failure 与最终 discarded pending 一并带入统一生命周期摘要，供上层诊断正常运行中的记录缺口。详情热路径只发布低基数计数和安全原因，状态由 `DnsService` 共享，不因普通 Runtime reload 重置。

Policy Core 通过 `DnsCore::resolve_with_observation` 提供已经完成策略判定的低基数 `strategy_id`、answer `source`、`cache_status`、`client_bucket`、独立的策略目标 `upstream_id` 与实际顶层 `upstream_member_id`，以及不含规则文本/matcher 的 matched rule/resource 摘要和 typed `ResourceVersion`；service 使用同一份 observation 写入现有 stats 维度与 resolve detail，避免从请求字段推测命中结果。client bucket/upstream/member/resource 仅接受已验证配置 ID，未知匹配不写入；stats 继续按实际成员优先聚合，`resolve_log` 则分别保存目标与成员存在性、规则来源、资源存在性、ordinal 和 `epoch:revision`。配置 ID 在详情表中统一脱敏为 `<present>`，不复制原值。

## 10. Flush 与 shutdown

shutdown：

1. 停止接收新 detail event；
2. 冻结最后一个 stats epoch；
3. 在 deadline 内提交 pending stats batch；
4. 尝试提交 detail batch；
5. WAL checkpoint；
6. 关闭 pool；
7. 返回已提交、丢弃和可能 gap 的摘要。

stats 优先级高于 detail。deadline 不足时先保证 ledger 一致性。

## 11. Migration

- migration 文件只前进，不在运行时自动 down；
- 每个 migration 在空库和上一版本库测试；
- 破坏性表重建使用新表 → copy/validate → rename；
- schema version 与配置/cache/resource version 独立；
- migration 失败保留原库并阻止启动；
- backup/rollback CLI 属于后续独立契约。

`backend/migrations/0001_storage.sql` 固定 `storage_meta`、按日统计、批次 ledger 和 `resolve_log` 基础表；`0002_resolution_metadata.sql` 以 additive columns 补充 group member 和 matched rule/resource 摘要。`SqliteStorageBackend` 在 prepare 打开独立业务数据库后按版本自动执行 v1→v2 migration，历史详情的新列保持 `NULL`，不做推测性回填；新库走相同升级链。adapter 使用 WAL、`synchronous=NORMAL`、busy timeout 和单 operation lock；`InMemoryStorageBackend` 继续作为无外部依赖的 contract baseline。

SQLite 首轮 adapter 的 `execute` 在一个事务内处理 stats batch 与 resolve detail batch：stats 通过 `stats_batch_ledger` 的 payload hash 做幂等重试/冲突拒绝，详情写入先复用 `ResolveDetailRecord` 生成脱敏摘要，再使用绑定参数落库；`SqliteResolveDetailWriter` 通过 bounded `mpsc` 只做非阻塞入队，`SqliteResolveDetailWorker` 按上限批量取出并以独立事务提交，失败时保留 pending 记录。启用详情限制时，同一事务先按年龄和软阈值淘汰，再按 `max_records` 裁剪新批次，并返回 committed/evicted/dropped 摘要；worker shutdown 会在 deadline 内循环 drain 所有 pending batch，`run` 入口按 flush interval 周期执行并在取消前完成最终 drain。事务中任一 operation 失败都会整体回滚。数据库错误进入 degraded，degraded 状态仍允许有限操作重试，成功的 migration/execute/detail/checkpoint 会恢复 healthy；不可恢复 adapter 错误进入 failed，health probe 不会自动绕过 failed。migrate/execute/detail/checkpoint/shutdown 共用的串行 operation lock，以及完整数据库 future，均受调用方 deadline 约束。`StorageService` 现统一 stats/backend/detail 的 flush/shutdown 顺序，并保留 stats/detail/backend 的 typed error 上下文。`StorageRuntime` 已由 Application prepare 创建，`DnsService` 注册受监督的周期 flush task，并在服务 drain 后依次关闭 detail、stats 和 backend；stats pending batch/event 达到固定内存上限时保留活动 epoch 并升级 fatal；execute/detail transaction 内部返回 `Unavailable` 时也会更新 degraded 状态。测试通过 `InjectedSqliteFault::{Busy,DiskFull}` 复现 adapter 级故障，并通过真实 SQLite 写锁复现 Busy，均验证下一次成功操作恢复 healthy；真实 OS disk-full 和故障压力仍待后续切片。

## 12. 测试

- 新库、旧版本库、重复启动 migration；
- stats total/dimension upsert；
- batch commit 后崩溃与 retry 去重；
- event 跨午夜、late write；
- parallel/hosts/cache source 计数；
- detail enable/disable、队列满、软阈值、硬上限和 age；
- stats/detail 并发互不阻塞；
- busy、disk full、permission、corruption；
- shutdown deadline 和 gap summary；
- 业务 DB 与 cache DB 完全隔离。

## 13. 实现检查清单

- [x] 建立 SQLx pool/migration 首轮 adapter；
- [x] 建立业务 migration schema 与可替换 stats writer contract；
- [x] 实现内存 stats counters/checkpoint/epoch/ledger 领域边界；
- [x] 实现 stats SQLite schema/upsert/checkpoint writer 首轮 adapter；
- [x] 实现独立 resolve-log writer channel 与 SQLite batch flush 首轮 adapter；
- [x] 实现详情年龄淘汰、软阈值和硬上限首轮策略；
- [x] 提供 stats/backend/detail 统一 flush/shutdown facade，并固定 stats 提交、detail drain 均在 backend shutdown 之前；
- [x] 实现 StatsPersistenceWorker 的 epoch snapshot、BatchLedger 顺序提交和失败保留；
- [x] 实现 `StorageRuntime`、Supervisor 注册和 DnsService 首轮生产装配；
- [x] 配置 reload 后复用进程级 stats worker，并由新 Runtime core 延续统计累计；
- [x] Supervisor fatal task 返回错误前执行 Storage 有界 shutdown，保留最终提交机会；
- [x] 实现 stats pending batch/event 内存保护上限与 fatal 分类；
- [x] 实现 SQLite 首轮 degraded/recovery 状态转换；
- [x] 完成受 `cfg(test)` 限定的 Busy/DiskFull adapter fault 注入、Unavailable→Degraded 分类和成功恢复测试；
- [x] 接入 Policy Core 的 source/cache/strategy/client bucket/selected upstream observation，并仅为实际 DNS response 聚合完整 RCODE；
- [x] 消费详情 sink 的明确丢弃 disposition，按原因累计并限频报告 backpressure；
- [x] 将详情 queue-full/sink 错误发布为 Storage degraded/gap，并在后续 accepted 时恢复 Healthy；
- [x] 将详情前端队列的 flush/shutdown 结果纳入 `StorageRuntime` 统一生命周期摘要；
- [x] 在 telemetry 关闭前发布 Storage `Stopping` health 和纯计数 shutdown 摘要；
- [x] 通过 schema v2 拆分 `upstream_id`/`upstream_member_id`，并持久化 matched rule/resource 摘要；
- [x] 写入 DNS header RCODE，并从既有 outcome/cancellation 契约生成低基数失败和取消摘要；
- [x] 将命中资源的 typed `ResourceVersion` 传播至详情，并在 SQLite 边界写入 `epoch:revision`；
- [x] 完成真实 SQLite 写锁 Busy 故障复现及恢复；
- [x] 完成业务 SQLite 串行 operation lock 的 deadline 与 shutdown 超时测试；
- [x] 完成连接池耗尽时完整数据库 future 的 deadline 测试；
- [x] 以内存和 SQLite adapter 共用测试验证 `StorageBackend` 可观测行为契约；
- [ ] 完成真实 OS disk-full 故障复现；
- [x] 完成当前 stats/ledger、跨午夜、幂等重试和 persistence gap 测试；
- [ ] 完成 migration、压力和故障测试。

阶段证据：Storage focused tests 覆盖 migration schema 表/维度约束、stats batch 原子 upsert、幂等重试、payload 冲突和失败回滚；`storage::sqlite::tests` 覆盖新库升级链、v1→v2 additive migration、历史详情不回填、stats batch 幂等重试/reopen、RCODE/failure/cancellation、resource revision 与其他详情摘要列写入、bounded writer、容量/年龄淘汰、事务回滚、health/shutdown 及 adapter fault 注入恢复；`storage::resolve_log::tests` 覆盖摘要转换、脱敏和队列边界；Service 的 RCODE 定向测试验证完整统计 RCODE 与详情 header RCODE 只来自实际 DNS response，shared-service reload 定向测试验证新旧 Runtime 的真实 UDP 请求由同一 stats worker 连续累计，正常 shutdown 定向测试验证请求统计最终写入并可从 SQLite 重新读取。阶段 190 后端全量 `555 passed、0 failed`；阶段 183 使用独立连接持有真实 SQLite 写锁，验证 Busy 映射为 `Unavailable`、health 进入 `Degraded`，释放锁后的事务恢复 `Healthy`；阶段 187 使业务 SQLite 串行锁等待遵守 deadline；阶段 189 进一步限制完整数据库 future，并以连接池耗尽验证短 deadline 稳定返回 `Timeout`；阶段 194 以共享测试验证内存与 SQLite `StorageBackend` 契约，`2 passed、0 failed`。

阶段 131 的 `StorageRuntime` 定向测试已验证详情事件从前端队列提交至 SQLite worker，且统一 flush/shutdown 摘要保留前端 committed 与 discarded pending 计数。

阶段 133 复用受监督 StorageRuntime 停机测试，验证 service drain 后可正常取得统一摘要；生产路径会在 Telemetry 关闭前输出 stats/backend/resolve-log/detail 的安全计数，不记录请求内容。阶段 140–142 拆分并落库策略目标、实际顶层成员和 matched rule/resource 摘要；阶段 143–144 分别补齐聚合统计的完整 RCODE，以及详情表的 header RCODE、failure class 和 cancellation reason；阶段 148 将匹配时的资源 epoch/revision 写入既有 `resource_revision` 列。阶段 153 补齐详情 writer 的 queue-full/sink-error degraded health 和 accepted recovery，5 项 Service 定向测试覆盖状态字段、Storage 生命周期与 reload；阶段 159 验证 Runtime reload 前后的新旧 core 复用同一进程级 stats worker；阶段 160 验证真实 UDP 请求在正常 Service shutdown 后完成 SQLite 最终提交；阶段 161 验证 Supervisor fatal task 退出前也完成同一最终提交。

当前实现进度：**99%**（已完成内存 stats/ledger、schema v2、SQLite stats/detail transaction、幂等提交、脱敏详情与策略元数据、bounded writer、详情 backpressure health/recovery、跨 Runtime stats worker 复用、统一 flush/shutdown 及正常/fatal task 停机落盘验收、pending 内存保护、adapter-level Busy/DiskFull 注入、真实 SQLite Busy 复现和完整数据库 future deadline；真实 OS disk-full 和故障压力测试仍未完成）。
