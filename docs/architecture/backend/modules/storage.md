# Storage 模块设计

> 文档状态：有效
>
> 适用范围：SQLite、统计、解析记录、migration、容量边界和存储生命周期
>
> 最后评审：2026-09-05（模块边界与关键契约静态核对，基线见[模块索引](README.md)；不含运行验收）
>
> 关联实现：[sqlite.rs](../../../../backend/src/storage/sqlite.rs)、[service.rs](../../../../backend/src/storage/service.rs)、[statistics.rs](../../../../backend/src/storage/statistics.rs)、[ledger.rs](../../../../backend/src/storage/ledger.rs)、[migrations](../../../../backend/migrations)
>
> 关联文档：[后端架构](../overview.md) · [配置字段参考](../../../implementation/configuration.md) · [Ports](ports.md) · [Observability](observability.md) · [Cache](cache.md)

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
| `sqlite.rs` | SQLx pool、PRAGMA、migration、统计/详情 transaction、唯一 bounded detail writer、满批/周期 flush、详情淘汰/硬上限、health/checkpoint/shutdown |
| `service.rs` | `StorageRuntime` 组装、详情 worker task、backend/detail flush 与 shutdown 顺序 facade、resolution metrics owner |
| `stats.rs` | StatsAccumulator epoch snapshot、BatchLedger 顺序提交与失败重试 worker |
| `statistics.rs` / `ledger.rs` | sharded counters/epoch checkpoint 与 pending batch ledger |
| `resolve_log.rs` | 从 typed `ResolutionEvent` 投影、校验和裁剪 `ResolveDetailRecord` |
| `management_read.rs` | Management overview、统计和解析详情的独立只读 SQLite adapter、安全投影与固定查询模板 |
| `writer.rs` | 无外部依赖的事务/幂等 writer contract 实现与 focused tests |

## 2. SQLite 初始化

prepare 阶段：

1. 创建数据库父目录；
2. 以读写/创建模式打开文件；
3. 显式设置 WAL、2 秒 busy timeout，连接池最多 4 个连接；
4. 使用 `synchronous=NORMAL` 作为吞吐与崩溃恢复折中；
5. 通过 `include_str!` 内嵌 SQL 手动创建基础表，再按 `storage_meta.schema_version` 执行前向 migration；不是 `sqlx::migrate!`/SQLx Migrator；
6. `StorageRuntime::open` 调用 `migrate` 核对当前 schema version，没有单独的写入/回滚探针；
7. 建立 stats worker 和唯一的 SQLite detail writer channel；
8. 返回 `StorageRuntime`。

connect/schema/migration 失败属于启动 fatal；已有库能打开和通过版本核对不等于已执行专门的写入可用性测试。当前 connect/migration 本身未整体包裹调用方 deadline，后续 port 操作才使用该预算；启动探针和总预算差距见[核对计划](../../../plans/backend-contract-gaps.md)。

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

- event time、从 transport 接入到 core 完成的 request duration，以及微秒精度的 DNS core 主链耗时；
- request ID digest；
- listener、route 存在性、配置 client bucket、有效 client IP 和 strategy；
- canonical qname、qtype、qclass；
- 策略目标 upstream/group、实际产生响应的 direct/顶层 group member；cache hit 保存缓存生产来源；
- matched rule 来源、资源存在性标记和可选 ordinal；
- 当前 schema 没有 ECS mode/prefix 列，不能宣称已持久化 ECS 诊断；
- source、RCODE、cache status 和有界 answer JSON（最多 16 条、4096 bytes）；
- failure/cancellation 分类；
- runtime/resource revision 摘要。

解析详情本身是敏感数据；数据库文件使用工作目录权限保护，不把详情复制到服务日志。

## 4. 聚合统计热路径

DNS 请求任务只做一次有界 `ResolutionEnvelope::try_publish`。进程级 dispatcher 接受事件后才更新内存 sharded counters：

- 单请求分配 monotonic event sequence；
- `day_utc` 在事件发生时确定；
- 一次请求只增加一次 total；
- 维度 key 由 typed enum/ID 构造；
- `attempt_outcome` 维度来自同一请求终态，不是逐 parallel attempt 计数；实际 upstream attempt 没有独立统计入口。

producer 发布不得 await、锁 SQLite 或构造详情字符串；dispatcher 的 stats 更新也不得等待 SQLite。统一 ingress 满时，该事件未进入聚合计数，`accepted/dropped/gap_started_at_utc_millis` 会明确暴露这个前置 gap。

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

当前 `StatsPersistenceWorker` 已实现上述闭环：resolution dispatcher 通过 `StatsRecorder` 只触碰内存 accumulator，`flush` 先冻结 epoch，再将 pending batch 通过 `StorageBackend::execute` 按 batch ID 顺序提交；backend 失败时仅增加 batch 的失败尝试次数并保留原 payload，后续 flush 可继续幂等重试。worker 同时返回 committed batch/event 数量、pending 数量和 persistence gap 摘要。`StorageService` 普通 flush 按 stats → backend checkpoint → detail 执行，shutdown 按 stats → detail drain → backend close 执行。

运行期间未提交批次与 ingress 丢弃有可观测 gap；进程硬崩溃会丢失尚未落库的计数，当前没有请求 WAL 或重启后恢复丢失数量的机制，不承诺能重建或准确报告这部分数量。

## 6. Resolve log writer

详情开启时，resolution producer 在统一事件中附带 typed `ResolutionDetailSource`，后台 dispatcher 再尝试写入独立的 projection channel：

- projection send 使用 non-blocking `try_send`；
- projector 在后台生成 request digest、canonical qname 和有界 answer JSON，再调用 SQLite writer 的 non-blocking `try_write`；
- projection 或 SQLite queue 满时只丢弃当前详情，并分别累计 `detail_dropped`；写入拒绝累计 `detail_failed`；
- SQLite worker 达到 batch 上限时立即提交，低流量尾批最多等待 5 秒；
- `enable=false` 时 producer 不附带详情 source，但同一低基数事件仍进入 stats/cache 消费者。

请求级字符串化和字段长度限制只在 projector/SQLite 边界执行；总耗时与主链耗时在 DNS core 完成时已冻结为数值，projector 不再根据当前时刻计算。`ResolutionEvent` 的 `Debug` 只显示存在性和 typed 安全字段。

## 7. 淘汰和硬上限

每次详情 batch commit 在同一维护循环检查：

1. 删除早于 `max_record_age` 的记录；
2. 数量达到 `eviction_threshold_records` 时按时间/id 删除最旧记录；
3. 目标降到软阈值以下；
4. 插入前计算本事务后的数量；
5. 如果仍会超过 `max_records`，丢弃本批次中最晚到达的详情并计数。

硬上限判断和插入在同一 writer 串行路径中完成，避免并发突破。聚合表和 ledger 不受详情上限影响。

## 8. Connection 与事务

- stats 和 detail 使用同一业务数据库，但独立逻辑 worker；
- pool 保持小规模，避免 SQLite 写锁竞争；
- 两个 worker 的事务短且不在 DNS 请求任务中执行；
- Management 只读查询使用独立、最多两个连接的 read-only pool；
- 所有 SQL 使用 bind 参数；
- migration 只在 prepare 执行。

## 9. 运行期故障

SQLite busy、磁盘满、I/O error：

- stats：保留未 ack batch，继续内存计数，进入 degraded；
- resolution ingress：队列满时整条事件丢弃，累计 `dropped` 与首次 gap 时间，并通过 `Component::Resolution` 发布 degraded；后续 accepted 可恢复 Healthy，但历史 gap 计数不清零；
- detail：projection/SQLite queue 与写入失败只按 `detail_dropped`/`detail_failed` 分类，不影响已进入 dispatcher 的 stats；
- DNS 请求继续服务；
- 恢复后 stats 按 batch ledger 幂等补写；
- 记录 degraded 首发、最近重试、积压 batch、persistence gap 风险；
- pending batch/补偿计数达到 v1 固定内存保护上限时，stats 不能静默丢弃；升级为明确 fatal 或受控进程退出，由 supervisor 处理。

`StorageBackend` 自身 panic、schema corruption 或无法保证 ledger 正确性时升级 fatal，不继续写可能重复的统计。

resolution runtime 与 Storage 都由进程级 owner 持有，不因普通 Runtime reload 重置。Management overview 暴露 ingress accepted/dropped/首次 gap、cache commit 各终态和 detail accepted/dropped/failed；Storage shutdown 摘要继续报告 SQLite detail committed/evicted/dropped 和 stats persistence 状态。

Policy Core 通过 `DnsCore::resolve_with_completion` 提供已经完成策略判定的 `strategy_id`、answer `source`、lookup `cache_status`、`client_bucket`、策略目标 `upstream_id`、实际结果 `upstream_used_id`，以及不含规则文本/matcher 的 matched rule/resource 摘要和 typed `ResourceVersion`；service 将其与最终共享 `CoreOutcome` 组合成唯一 `ResolutionEvent`。cache hit 从 `CacheEntry` 恢复生产请求的 target/used provenance，不以当前 route 猜测。stats 仅消费低基数维度；detail projector 允许保存已验证配置 ID、canonical qname、有效 client IP 和有界 answer，但这些请求级值不进入事件 `Debug`、tracing 或 telemetry label。

## 10. Flush 与 shutdown

shutdown：

1. `DnsService` 先停止接收新请求并完成 request drain；
2. 停止并排空 resolution ingress、cache commit 和 detail projection worker；
3. `StorageRuntime` 停止 SQLite detail worker，使其提交最终 batch；
4. 冻结最后一个 stats epoch，在 deadline 内提交 pending stats batch；
5. WAL checkpoint 并关闭 pool；
6. 返回 resolution、stats、detail 和可能 gap 的独立摘要。

生产 `StorageRuntime` 先等待独立 detail task，再调用 `StorageService::shutdown` flush stats/backend；用于内联 worker 的 StorageService facade 才是 stats → detail → backend。不能把后者的顺序描述为生产停机始终“stats 优先”；总预算不足时这一差异仍需验收。幂等 ledger 必须在各自事务内保持一致。

## 11. Migration

- migration 文件只前进，不在运行时自动 down；
- 每个 migration 在空库和上一版本库测试；
- 破坏性表重建使用新表 → copy/validate → rename；
- schema version 与配置/cache/resource version 独立；
- migration 失败保留原库并阻止启动；
- backup/rollback CLI 属于后续独立契约。

新库与旧库走同一前向升级链。新增可空详情字段不补造历史事实；历史脱敏记录由 read model 明确标为 legacy_redacted，缺失主链耗时保持 null。实际 migration 文件和 schema 版本见[后台服务实现](../../../implementation/backend/background-services.md)，不在设计中重复逐版本清单。

只读 Management pool 必须在业务 migration 完成后创建，通过 ManagementStorageRead 使用固定 filter/sort 模板与参数绑定。返回 opaque ID，不暴露数据库 row ID、wire、request digest 或内部脱敏占位符。详情校验/裁剪在受限 projector/writer 边界完成，Debug 只展示存在性、长度和计数。

## 12. 契约验证要求

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
- Management read-only pool、分页/filter/sort、opaque ID 和敏感字段安全投影。
