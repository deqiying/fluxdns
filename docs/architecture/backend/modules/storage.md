# Storage 模块设计

> 文档状态：有效
>
> 适用范围：统计 SQLite、解析详情日分片、migration、lease 和存储生命周期
>
> 最后评审：2026-09-08（BC-13 对外历史 API 接入跨日读口）
>
> 关联实现：[detail_shards.rs](../../../../backend/src/storage/detail_shards.rs)、[detail_query.rs](../../../../backend/src/storage/detail_query.rs)、[retention.rs](../../../../backend/src/storage/retention.rs)、[sqlite.rs](../../../../backend/src/storage/sqlite.rs)、[service.rs](../../../../backend/src/storage/service.rs)、[statistics.rs](../../../../backend/src/storage/statistics.rs)、[ledger.rs](../../../../backend/src/storage/ledger.rs)、[migrations](../../../../backend/migrations)
>
> 关联文档：[后端架构](../overview.md) · [配置字段参考](../../../implementation/configuration.md) · [Ports](ports.md) · [Observability](observability.md) · [Cache](cache.md)

## 1. 职责与边界

Storage 模块实现两个相互隔离的持久化 owner：

- 当前布局初始化与版本拒绝；
- 默认开启的聚合统计；
- 可选解析详情 UTC 日分片；
- writer 健康状态、flush 和 shutdown。

它不存储 DNS response cache。Cache persistence 使用配置中的独立 `FDCS` 文件和进程级 `CacheSnapshotOwner`，不能复用本模块 pool、表或 writer。

内部结构：

| 文件 | 职责 |
| --- | --- |
| `detail_shards.rs` | 日分片 layout、日期路径、受限连接 registry、读写/退役 lease、唯一 bounded 生产 detail writer 与关闭边界 |
| `detail_query.rs` | 稳定 opaque 记录 ID、进程绑定 keyset cursor、跨分片有界读取、retention revision 和 commit cursor/通知基础 |
| `retention.rs` | R/G/T 纯计算、受管文件大小采样、共同水位发布，以及 stats/detail owner 的一致性边界 |
| `sqlite.rs` | 统计 SQLx pool、当前布局初始化/拒绝旧库、统计 transaction、health/checkpoint/shutdown；日分片复用有界 insert helper |
| `service.rs` | `StorageRuntime` 组装、分片详情 worker task、统计 backend/detail store 的 shutdown 顺序、resolution metrics owner |
| `stats.rs` | StatsAccumulator epoch snapshot、BatchLedger 顺序提交与失败重试 worker |
| `statistics.rs` / `ledger.rs` | sharded counters/epoch checkpoint 与 pending batch ledger |
| `resolve_log.rs` | 从 typed `ResolutionEvent` 投影、校验和裁剪 `ResolveDetailRecord` |
| `writer.rs` | 无外部依赖的事务/幂等 writer contract 实现与 focused tests |

## 2. Storage 初始化

prepare 阶段先初始化统计库：

1. 创建数据库父目录；
2. 以读写/创建模式打开文件；
3. 显式设置 WAL，busy timeout 取 2 秒与剩余启动预算的较小值，连接池最多 4 个连接；
4. 使用 `synchronous=NORMAL` 作为吞吐与崩溃恢复折中；
5. 空库在同一事务创建 `fluxdns_layout(kind=statistics-v2, layout_version=1)`、基础表和 metadata；已有库必须先带匹配标记，未标记旧 schema 或错误标记直接拒绝；直接创建或核对当前 `storage_meta.schema_version=9`，不执行旧版本迁移；
6. `StorageRuntime::open` 调用 `migrate` 核对当前 schema version，在独立事务内更新 singleton metadata 并显式回滚，验证真实写入路径；
7. 建立 stats worker；
8. 校验详情受管目录与统计库、缓存快照不存在词法包含或物理文件别名，再建立最多 4 个活动连接的分片 registry；
9. 详情启用时建立唯一的分片 writer channel；未启用或尚无记录时不创建详情目录；
10. 使用 `statistics.retention` 的 R/G/T 创建唯一 retention scheduler，再返回 `StorageRuntime`。

详情第一次写某个事件 UTC 日时，registry 仅由已解析的 `database.records_path` 和日期生成 `YYYY-MM-DD.sqlite3`，以 WAL、`synchronous=NORMAL`、单连接 pool 打开，在同一事务创建 `detail_meta`、`resolve_log`、日归属 trigger、时间/耗时索引及 client ID/IP、历史匹配 ID、qname 查询索引。已有文件必须声明匹配的 layout version/day 且具备完整 schema；普通外部 SQLite、错误日期 metadata、symlink/reparse point、hard link 及统计/缓存文件别名均拒绝采用。不会扫描或迁移旧单库详情。

建目录、connect/schema 校验、写探针共用调用方 deadline，不逐阶段重置。探针不提交业务统计或详情，也不永久修改 metadata；失败或预算耗尽不创建可服务的 Storage owner，并作为启动错误返回。deadline 限制异步等待与后续步骤，不承诺强制中断已进入 OS/SQLite worker 的操作；真实介质故障仍需环境验收。

## 3. Schema 职责

具体 SQL 放入 migration，逻辑表至少包括以下几类。业务绝对时间统一为 Unix epoch 起算的 UTC 毫秒 `INTEGER`，列名以 `_utc_millis` 明确单位；writer 绑定 `i64`，数据库约束非负且实际存储类型为 integer。亚毫秒截断、epoch 前归零沿用旧写入边界，超过 `i64` 上限明确拒绝，不静默截断为其他时间。

时间戳与耗时/日桶分开：`day_utc` 仍是 epoch 起算的 UTC 自然日编号，不是毫秒或 `YYYYMMDD`；`duration_millis`/`dns_core_duration_micros` 保留各自精度。API 展示层继续格式化日期，不因数据库改型改变 OpenAPI 时间字段。具体列清单见[后台服务实现](../../../implementation/backend/background-services.md#业务时间存储)。

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

### 分片 `detail_meta`

- 固定 singleton；
- detail layout version；
- 文件唯一归属的 `day_utc`；
- 创建时间。

### 分片 `resolve_log`

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

每个文件只接受 `event_time_utc_millis / 86400000 == detail_meta.day_utc` 的记录，应用路由错误也会由 SQLite trigger 回滚。分片内 `id` 只是局部自增键；`DetailRecordId` 将 layout、UTC 日和事务实际返回的 row ID 编码为带完整性校验的稳定 opaque token，重启后保持不变且不接受路径文本或被修改的 token。调用方不能将局部 ID 对外解释为全局 ID。

解析详情本身是敏感数据；受管目录使用工作目录权限保护，不把详情复制到服务日志。统计库 schema v9 中的旧 `resolve_log` 表暂留供旧读 adapter/兼容测试使用，生产 `StorageRuntime` 不再写入，待 BC-27 删除。

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

当前 `StatsPersistenceWorker` 已实现上述闭环：resolution dispatcher 通过 `StatsRecorder` 只触碰内存 accumulator，`flush` 先冻结 epoch，再将 pending batch 通过 `StorageBackend::execute` 按 batch ID 顺序提交；backend 失败时仅增加 batch 的失败尝试次数并保留原 payload，后续 flush 可继续幂等重试。worker 同时返回 committed batch/event 数量、pending 数量和 persistence gap 摘要。分片详情有自己的 store/lease，不再与统计事务争用同一个 pool。

运行期间未提交批次与 ingress 丢弃有可观测 gap；进程硬崩溃会丢失尚未落库的计数，当前没有请求 WAL 或重启后恢复丢失数量的机制，不承诺能重建或准确报告这部分数量。

## 6. Resolve log writer

详情开启时，resolution producer 在统一事件中附带 typed `ResolutionDetailSource`，后台 dispatcher 再尝试写入独立的 projection channel：

- projection send 使用 non-blocking `try_send`；
- projector 在后台生成 request digest、canonical qname 和有界 answer JSON，再调用分片 writer 的 non-blocking `try_write`；
- projection 或 SQLite queue 满时只丢弃当前详情，并分别累计 `detail_dropped`；写入拒绝累计 `detail_failed`；
- 分片 worker 达到 batch 上限时立即提交，低流量尾批最多等待 5 秒；单个事务只取队首同一 UTC 日的连续记录，跨文件不做部分提交；
- `enable=false` 时 producer 不附带详情 source，但同一低基数事件仍进入 stats/cache 消费者。

请求级字符串化和字段长度限制只在 projector/SQLite 边界执行；总耗时与主链耗时在 DNS core 完成时已冻结为数值，projector 不再根据当前时刻计算。`ResolutionEvent` 的 `Debug` 只显示存在性和 typed 安全字段。

## 7. 保留边界

生产分片批写只执行有界入队、字段校验和 `INSERT`，不执行历史 `COUNT`、按条数淘汰、按年龄 `DELETE` 或 `VACUUM`。v2 已删除 `eviction_threshold_records`、`max_records`、`max_record_age`；旧字段只由 BC-27 待删除的测试 loader/单库 adapter 覆盖，不代表生产契约。

BC-10 按冻结的 `reference_day_utc` 与受管详情大小 `S` 计算共同水位：`S > T` 取 R 天，否则取 R+G 天，等于阈值仍享有宽限；保留范围包含当前 UTC 日，只退役严格早于 `keep_from_day_utc` 的数据。策略限制为 R 至少 1 天、R+G 最多 3650 天、T 为 1 byte 至 1 TiB，大小只累计规范详情主文件与 WAL，不计 stats、cache、SHM、备份或其他文件；采样失败或整数溢出会终止本轮，不解释为 0。

发布先取得全局详情 retention write lease，等待现有读写 lease 排空并阻止新 lease，再冻结 stats flush 的最早可重放 batch ID。schema v9 在单一 stats 事务中推进单调 `retention_state`、删除旧 stats 日、按 replay floor 而非日期删除 ledger，并把已存在的旧详情日登记到 `retention_detail_manifest`；事务成功后才在仍持有详情 write lease 时发布进程水位。失败事务不改变详情可见范围；期限延长或压力下降只更新运行记录，不后退水位或恢复数据。stats pending 重放在事务内读取水位，已退役事件只推进原 batch 的幂等确认，不增加业务计数；详情迟到批次计入 dropped 且不能重建旧分片。

`StorageRuntime::open` 从 stats DB 恢复水位，并以 `max(ledger high + 1, replay_floor)` 续接 batch ID，随后启动详情 writer 和唯一 retention scheduler owner。scheduler 每分钟重新读取系统时区与墙钟，不固定 sleep 24h；本地时间首次达到或越过 01:00 时，以当时 UTC 日运行一次。`retention_run_state` 持久化最后成功本地日，因此 DST 跳时会补跑、重复小时/回拨不会重复，时区变更按新本地日判断，失败五分钟后重试，重启会立即核对补跑。全新空库在 01:00 前以昨日为基线、在当日 01:00 首跑；01:00 后首次启动以当日为基线，不伪造无数据清理。

共同水位发布后，scheduler 对 pending/failed manifest 逐日取得退役独占 lease，等待内部读写连接排空，校验 layout 后执行 `wal_checkpoint(TRUNCATE)`、关闭 pool，再只删除规范主文件及 `-wal`/`-shm`/`-journal` sidecar。删除成功后标记 reclaimed；任何路径、checkpoint、关闭或删除失败都增加 attempts、保存安全错误码并保留重试资格。状态查询同时返回策略/目标天数、采样大小、已发布截止日、下一预计截止日、stats/detail 实际可查日范围、最后成功清理时间以及 pending/failed 数量。详情关闭时 scheduler 仍运行并保留 stats；生产 owner 使用 v2 `statistics.retention` 的 R/G/T。

BC-09 的历史 cursor 绑定规范化后的 filter、sort、order、翻页方向、当前 retention revision 和进程随机 key；任一上下文改变、进程重启、token 被修改或水位推进都会拒绝继续使用。`older` 沿当前排序继续，`newer` 反向扫描后恢复同一展示顺序；每个分片先用 bind 参数执行时间和业务过滤、keyset 条件及 `page_size + 1` 上限，随后只保留全局有界候选，不使用 `OFFSET`、`COUNT` 或分页后过滤。范围最多 3650 天，缺失日只检查路径且不创建目录/SQLite。

查询快照先捕获独立 `stream_epoch + sequence`。详情事务从每次 INSERT 的实际结果取得 row ID，commit 成功后才递增 sequence 并向有界 broadcast channel 发布该批稳定 ID 与安全 `ResolveDetailRecord`；迟到事件仍按提交序列被发现，失败事务和入队前丢弃不会发布。BC-09 不实现 replay、慢消费者处理或 WS 传输，这些仍归 BC-25；当前客户端目录名称和 `directory_revision` 也不在 Storage 中伪造，由 BC-13 在一次配置目录快照中投影。

## 8. Connection 与事务

- stats 使用主业务 SQLite pool；detail 每个 lease 使用单日、最多一个连接的临时 pool，两者不共享文件；
- registry 全局最多允许 4 个活动详情连接，同一天以日锁串行；历史日不常驻连接；
- read lease 对缺失文件返回空且不创建目录，retirement 先发布逻辑不可见再等待既有日锁；
- 两个 worker 的事务短且不在 DNS 请求任务中执行；
- 新 `DetailShardStore` 读口通过受限 read lease 跨分片查询，BC-13 正式 v2 API 已接入；旧 v1 Management HTTP 仍读取主库；
- 所有 SQL 使用 bind 参数；
- 统计 migration 只在 prepare 执行，当前 schema v9；日分片仅在首个写 lease 初始化并核对固定 layout。

## 9. 运行期故障

SQLite busy、磁盘满、I/O error：

- stats：保留未 ack batch，继续内存计数，进入 degraded；
- resolution ingress：队列满时整条事件丢弃，累计 `dropped` 与首次 gap 时间，并通过 `Component::Resolution` 发布 degraded；后续 accepted 可恢复 Healthy，但历史 gap 计数不清零；
- detail：projection/SQLite queue 与写入失败只按 `detail_dropped`/`detail_failed` 分类，不影响已进入 dispatcher 的 stats；
- DNS 请求继续服务；
- 恢复后 stats 按 batch ledger 幂等补写；
- 记录 degraded 首发、最近重试、积压 batch、persistence gap 风险；
- pending batch/补偿计数达到 v1 固定内存保护上限时，stats 不能静默丢弃；升级为明确 fatal 或受控进程退出，由 supervisor 处理。

`StorageBackend` 自身 panic、schema corruption 或无法保证 ledger 正确性时升级 fatal，不继续写可能重复的统计。分片文件 metadata/layout 不匹配、路径身份异常或连接 deadline 耗尽会拒绝该批详情，不静默接管外部文件；已进入 dispatcher 的统计仍独立处理。

resolution runtime 与 Storage 都由进程级 owner 持有，不因普通 Runtime reload 重置。Management overview 暴露 ingress accepted/dropped/首次 gap、cache commit 各终态和 detail accepted/dropped/failed；Storage shutdown 摘要继续报告 detail committed/evicted/dropped 和 stats persistence 状态。分片 writer 的 `evicted`/`dropped` 只保留统一摘要形状，正常批写固定为 0；队列拒绝仍由 resolution metrics 计量。

Policy Core 通过 `DnsCore::resolve_with_completion` 提供已经完成策略判定的 `strategy_id`、answer `source`、lookup `cache_status`、请求期 `ClientMatchObservation`、过渡期 `client_bucket`、策略目标 `upstream_id`、实际结果 `upstream_used_id`，以及不含规则文本/matcher 的 matched rule/resource 摘要和 typed `ResourceVersion`；service 将其与最终共享 `CoreOutcome` 组合成唯一 `ResolutionEvent`。cache hit 从 `CacheEntry` 恢复生产请求的 target/used provenance，不以当前 route 猜测。客户端匹配事实保存匹配来源和当时的稳定 ID，事件消费或 reload 不按当前目录重映射；stats 只使用该低基数 ID。detail projector 允许保存受限原始 client ID、有效 client IP、匹配事实、已验证配置 ID、canonical qname 和有界 answer，但这些请求级值不进入事件 `Debug`、tracing 或 telemetry label。

## 10. Flush 与 shutdown

shutdown：

1. `DnsService` 停止接收新请求、尽快取消在途 dispatch 并回收 request guard，不保证已读请求一定写回；
2. 停止并排空 resolution ingress、cache commit 和 detail projection worker；
3. `StorageRuntime` 关闭 detail 输入，等待当前正在写入的 batch 结束，取回尚未排空的 worker；
4. 冻结最后一个 stats epoch，优先提交 pending stats batch；
5. 详情只使用剩余预算按日排空，每个写 lease 关闭其 pool；
6. detail store 拒绝新 lease并等待全部活动连接归还，再完成统计 backend 关闭结果汇总；
7. 返回 resolution、stats、detail 和可能 gap 的独立摘要。

生产 owner 先停止详情输入并等待当前分片事务返回，再由 `StorageService` 提交 stats/关闭主 backend，随后用剩余预算排空已取回的详情 worker 并关闭 registry。已执行的 SQLite 写入仍需先结束，可能消耗剩余预算，因此“统计优先”不等于抢占正在进行的 SQL 或保证零丢失。全部阶段共享同一 deadline，超时/失败显式报告，幂等 ledger 在统计事务内保持一致。

## 11. Migration

- migration 文件只前进，不在运行时自动 down；
- 每个 migration 在空库和上一版本库测试；
- 破坏性表重建使用新表 → copy/validate → rename；
- schema version 与配置/cache/resource version 独立；
- migration 失败保留原库并阻止启动；
- backup/rollback CLI 属于后续独立契约。

当前统计库仍走原前向升级链并保留旧详情表；BC-08 不迁移、复制、删除或重新匹配旧详情。新日分片仅接受 layout v1 空文件或由 owner 新建的文件，不把单库旧行搬入分片。正式新数据基线和旧格式拒绝由 BC-26 完成。

新增可空详情字段不补造历史事实；历史脱敏记录由 read model 明确标为 legacy_redacted，缺失主链耗时保持 null。升级会一次性复制相关表并重建时间索引，需要额外临时空间，仍使用原启动 deadline；不擅自延长预算。旧 binary 不支持新 schema，不自动 down。实际 migration 文件和 schema 版本见[后台服务实现](../../../implementation/backend/background-services.md)，不在设计中重复逐版本清单。

只读 Management pool 必须在业务 migration 完成后创建，通过 ManagementStorageRead 使用固定 filter/sort 模板与参数绑定。范围过滤、时间排序和详情清理直接比较整数时间列，不再依赖逐行 `CAST` 或文本字典序。返回 opaque ID，不暴露数据库 row ID、wire、request digest 或内部脱敏占位符。详情校验/裁剪在受限 projector/writer 边界完成，Debug 只展示存在性、长度和计数。

## 12. 契约验证要求

- 新库、旧版本库、重复启动 migration；
- 时间改型无损复制、异常值回滚、实际 INTEGER 类型、自增高水位、跨位数排序/范围/清理与索引使用；
- stats total/dimension upsert；
- batch commit 后崩溃与 retry 去重；
- 真实日分片空目录、event 跨午夜、late write 与日归属 trigger；
- parallel/hosts/cache source 计数；
- detail enable/disable、队列满、同日串行、活动连接上限和关闭排空；
- 生产批写不执行条数/年龄淘汰，v1 小上限不截断分片记录；
- stats/detail 文件隔离，生产主库旧详情表保持 0 新写入；
- busy、disk full、permission、corruption；
- shutdown deadline 和 gap summary；
- 统计 DB、详情目录与 cache 文件完全隔离。
- BC-09 已覆盖跨分片分页/filter/sort、opaque ID、cursor 水位/完整性和提交通知时序；BC-13 已覆盖当前目录名称安全投影、分页前过滤与 Bearer HTTP。
- BC-10 已覆盖 R/G/T 阈值/边界、主文件+WAL 采样、共同水位单调性、stats pending/详情迟到保护、manifest/ledger replay floor、事务回滚、lease 排空和启动恢复。
- BC-11 已覆盖 01:00 前后、DST 跳过/重复、墙钟回拨、时区日期变化、失败 retry gate、跨重启单日一次、真实多日 checkpoint/delete、删除失败 manifest 重试、cache 文件隔离和运行状态查询；未执行 Linux 实机或真实权限/磁盘满。
