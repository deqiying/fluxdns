# Storage 模块设计

> 文档状态：有效
>
> 实现状态：部分实现
>
> 适用范围：SQLite、统计、解析记录、migration、容量边界和存储生命周期
>
> 最后核对：2026-09-04
>
> 关联实现：`backend/src/storage/*`、`backend/migrations/*`
>
> 关联文档：[后端架构](../architecture.md) · [配置字段参考](../configuration-reference.md) · [Ports](ports.md) · [Observability](observability.md) · [Cache](cache.md)

## 当前实现边界

v1 方案已完成，已实现纯内存统计 epoch/batch ledger、业务 schema v5 migration、SQLx SQLite storage adapter、Management 独立只读 adapter、`StatsPersistenceWorker`、authenticated 查询详情投影、cache upstream provenance、bounded answer 摘要、group member、matched rule/resource、RCODE、failure 与 cancellation 摘要列、唯一的 bounded SQLite detail writer channel、后台 typed detail projector、满批立即提交/低流量定时提交、详情丢弃分类计数、年龄/软阈值/硬上限策略、worker shutdown drain、`StorageRuntime`/`DnsService` 生产接线、pending 内存保护/fatal 边界、degraded/recovery 状态转换和 fault injection。DNS 请求不再直接调用 stats/detail sink；两者由进程级 resolution dispatcher 消费同一个完成事件。真实 OS disk-full 复现、migration 压力与故障测试和跨故障源 telemetry 闭环尚未完成。

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
| `statistics.rs` | sharded counters、checkpoint、batch ledger |
| `resolve_log.rs` | 从 typed `ResolutionEvent` 投影、校验和裁剪 `ResolveDetailRecord` |
| `management_read.rs` | Management overview、统计和解析详情的独立只读 SQLite adapter、安全投影与固定查询模板 |
| `writer.rs` | 无外部依赖的事务/幂等 writer contract 实现与 focused tests |

## 2. SQLite 初始化

prepare 阶段：

1. 创建数据库父目录；
2. 以读写/创建模式打开文件；
3. 设置 `foreign_keys=ON`、WAL 和有界 busy timeout；
4. 使用 `synchronous=NORMAL` 作为吞吐与崩溃恢复折中；
5. 运行嵌入 binary 的 SQLx migrations；
6. 执行小型写入/回滚探针；
7. 建立 stats worker 和唯一的 SQLite detail writer channel；
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

- event time、从 transport 接入到 core 完成的 request duration，以及微秒精度的 DNS core 主链耗时；
- request ID digest；
- listener、route 存在性、配置 client bucket、有效 client IP 和 strategy；
- canonical qname、qtype、qclass；
- 策略目标 upstream/group、实际产生响应的 direct/顶层 group member；cache hit 保存缓存生产来源；
- matched rule 来源、资源存在性标记和可选 ordinal；
- ECS mode/prefix length，不保存完整敏感 client ECS；
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
- parallel attempt 只进入 attempt counter，不重复 total。

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

进程在计数尚未进入 checkpoint 前崩溃会产生 in-memory persistence gap；系统必须报告，不承诺绝对无损。

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

stats 优先级高于 detail。deadline 不足时先保证 ledger 一致性。

## 11. Migration

- migration 文件只前进，不在运行时自动 down；
- 每个 migration 在空库和上一版本库测试；
- 破坏性表重建使用新表 → copy/validate → rename；
- schema version 与配置/cache/resource version 独立；
- migration 失败保留原库并阻止启动；
- backup/rollback CLI 属于后续独立契约。

`backend/migrations/0001_storage.sql` 固定基础表；`0002_resolution_metadata.sql` 补充 group member 和 matched rule/resource；`0003_management_query_projection.sql` 补充 transport；`0004_query_record_observability.sql` 新增有效 client IP、`upstream_used_id`、answer count/truncated/JSON；`0005_dns_core_duration.sql` 新增 `dns_core_duration_micros`。`SqliteStorageBackend` 在 prepare 后逐版本自动执行 v1→v5 migration；schema v5 之前的主链耗时不回填，新列保持 `NULL`。v4 之前的请求详情同样不回填，并由 read model 标记为 `legacy_redacted`。新库走相同升级链。adapter 使用 WAL、`synchronous=NORMAL`、busy timeout 和单 operation lock；`InMemoryStorageBackend` 继续作为无外部依赖的 contract baseline。

SQLite adapter 的 `execute` 在一个事务内处理 stats batch 与 resolve detail batch：stats 通过 ledger payload hash 做幂等重试/冲突拒绝，详情先由 `ResolveDetailRecord` 校验 qname、配置 ID、IP 并裁剪 answer，再使用绑定参数落库；`Debug` 只输出存在性、长度和计数。`SqliteResolveDetailWriter` 是进入 SQLite detail worker 的唯一 bounded `mpsc`，支持 clone 后由 projector 非阻塞 `try_write`；worker 满批立即提交，失败时保留 pending，低流量尾批由 5 秒周期 flush。年龄淘汰、软阈值、硬上限、shutdown drain、健康恢复和 deadline 语义保持不变。

`SqliteManagementReadModel` 在业务写库完成 migration 后以 read-only 模式另建小型 pool，通过 `ManagementStorageRead` 提供 overview、统计和 authenticated 解析详情投影。filter 与 sort 只从编译期固定模板选择，值全部绑定；v5 行返回 opaque ID、canonical qname、qtype、client name/IP、strategy、target/actual upstream、有界 answer 和主链耗时。v5 前记录的 `dns_core_duration_ms` 返回 `null`；v4 前仍可查询的脱敏行不会返回 `len:*`/`<present>`，而是以 `detail_status=legacy_redacted` 和 nullable 详情字段显式表示不可恢复。

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
- Management read-only pool、分页/filter/sort、opaque ID 和敏感字段安全投影。

## 13. 实现检查清单

- [x] 建立 SQLx pool/migration 首轮 adapter；
- [x] 建立业务 migration schema 与可替换 stats writer contract；
- [x] 实现内存 stats counters/checkpoint/epoch/ledger 领域边界；
- [x] 实现 stats SQLite schema/upsert/checkpoint writer 首轮 adapter；
- [x] 实现唯一的 SQLite resolve-detail writer channel、后台投影与满批/周期 flush；
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
- [x] 由统一 resolution dispatcher 更新 stats 并独立分发详情；请求任务不再直接调用 storage sink；
- [x] 将 qname digest、canonical qname、answer JSON 与字段裁剪移动到后台 detail projector；
- [x] 以 resolution ingress gap 和 detail downstream failure 两组独立计数暴露 backpressure；
- [x] 在 telemetry 关闭前发布 Storage `Stopping` health 和纯计数 shutdown 摘要；
- [x] 通过 schema v2 拆分 `upstream_id`/`upstream_member_id`，并持久化 matched rule/resource 摘要；
- [x] 通过 schema v3 增加 Management 查询的 transport 投影，并建立独立 read-only SQLite adapter；
- [x] 通过 schema v4 保存 canonical qname、有效 client IP、真实配置 ID、cache producer upstream provenance 和有界 answer，并兼容读取历史脱敏行；
- [x] 通过 schema v5 保存 DNS core 主链耗时，并对历史行返回 `null`；
- [x] 写入 DNS header RCODE，并从既有 outcome/cancellation 契约生成低基数失败和取消摘要；
- [x] 将命中资源的 typed `ResourceVersion` 传播至详情，并在 SQLite 边界写入 `epoch:revision`；
- [x] 完成真实 SQLite 写锁 Busy 故障复现及恢复；
- [x] 完成业务 SQLite 串行 operation lock 的 deadline 与 shutdown 超时测试；
- [x] 完成连接池耗尽时完整数据库 future 的 deadline 测试；
- [x] 以内存和 SQLite adapter 共用测试验证 `StorageBackend` 可观测行为契约；
- [ ] 完成真实 OS disk-full 故障复现；
- [x] 完成当前 stats/ledger、跨午夜、幂等重试和 persistence gap 测试；
- [ ] 完成 migration、压力和故障测试。

阶段证据：Storage focused tests 覆盖 migration schema 表/维度约束、stats batch 原子 upsert、幂等重试、payload 冲突和失败回滚；`storage::sqlite::tests` 覆盖新库升级链、历史详情不回填、stats batch 幂等重试/reopen、RCODE/failure/cancellation、resource revision、bounded writer、容量/年龄淘汰、事务回滚、health/shutdown 及 adapter fault 注入恢复；`storage::resolve_log::tests` 覆盖从 typed resolution event 的摘要转换和脱敏。阶段 199 新增 60 秒 timer 下“详情 batch 满即提交”的定向测试，1 秒内完成 SQLite commit；统一 pipeline 测试覆盖 stats 与 detail 下游隔离。阶段 200 补充 schema v5 主链耗时写入/读取、历史 `NULL` 和投影边界，后端全量 `599 passed、0 failed、1 ignored`。

阶段 133 复用受监督 StorageRuntime 停机测试，验证 service drain 后可正常取得统一摘要；生产路径会在 Telemetry 关闭前输出 resolution/stats/backend/detail 的安全计数，不记录请求内容。阶段 140–148 拆分并落库策略目标、实际顶层成员、matched rule/resource、RCODE、failure/cancellation 和资源版本；阶段 159–161 验证 Runtime reload 复用进程级 stats worker，并在正常/fatal 停机前最终提交。阶段 199 删除旧的独立详情发布前端，统一由 resolution dispatcher 投影到 SQLite writer。

当前实现进度：**99%**（已完成内存 stats/ledger、schema v5、SQLite stats/detail transaction、幂等提交、authenticated 查询详情、统一 resolution 事件消费、后台详情投影、唯一 bounded SQLite writer、跨 Runtime worker 复用、有序 shutdown、pending 内存保护、adapter-level Busy/DiskFull 注入、真实 SQLite Busy 复现和完整数据库 future deadline；真实 OS disk-full 和故障压力测试仍未完成）。
