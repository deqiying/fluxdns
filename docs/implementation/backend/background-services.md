# 后台服务实现

> 文档状态：有效
>
> 适用范围：资源刷新、解析完成事件、统计/详情、缓存持久化和观测的实际所有权
>
> 最后核对：2026-09-05（UTC；业务时间、升级矩阵、SQL/flush 停机交错与本机契约验证）
>
> 核对基线：`f65fb3f8bd68e1a40ca041d9a380859b44a3da0c` 加本次契约验证工作树
>
> 2026-09-06 增量核对：仅更新连接/负载驱动、命令和本次开发验证；其余正文保留上述历史核对范围
>
> 同日文档收口：经用户确认结束剩余验证专项，仅维护已知边界和引用；不新增运行结论，不重跑历史测试
>
> 2026-09-08 增量核对：仅核对客户端请求身份、历史匹配事实和 schema v7 前向迁移；其余正文保留原核对范围

## 资源准备与刷新

[`PreparedRuntime::prepare_with_policy_core_and_remote_resources`](../../../backend/src/runtime/prepared.rs) 装配 [`ReqwestResourceFetcher`](../../../backend/src/resource/fetcher.rs)，先形成 file hosts/rule-set 和 remote rule-set 的可用 snapshot，再创建 core。remote 数据由 [`remote.rs`](../../../backend/src/resource/remote.rs) 恢复或抓取并持久化，解析器在 [`resource/rules.rs`](../../../backend/src/resource/rules.rs) 与 [`hosts.rs`](../../../backend/src/resource/hosts.rs)，不是请求时解析原始文件。

`auto_update` 对应的 worker 由 [`service.rs`](../../../backend/src/service.rs) 的 `run_resource_refresh_loop` 纳入 Supervisor，通过 coordinator 查询当前活动实例。刷新在 per-resource epoch/CAS 边界发布，并合并 Policy 内容与 runtime metadata；旧候选/旧 runtime 结果不会直接覆盖新实例。失败保留旧 snapshot，stale/retry 由 [`scheduler.rs`](../../../backend/src/resource/scheduler.rs) 与 [`refresh.rs`](../../../backend/src/resource/refresh.rs) 管理。

Policy 的 matcher/version/hash 先一起发布，随后单独更新 Runtime metadata，不是跨对象原子事务。fetcher 支持有界 ETag/Last-Modified 条件请求，结果区分 `Modified`/`NotModified`；拒绝重定向和其余非成功状态。2xx body 有界读入内存，解析成功才保存 content/manifest。

manifest v2 保存源身份 digest、fetcher 代际及不透明验证器，不保存原始 URL。只有本地 pair 完整、身份及代际匹配才发送条件头；304 返回后再次检查 pair，更新 manifest 与成功状态，不重写 body。失去条件基础时最多补发一次同 deadline 的无条件请求，再次 304 明确失败。URL/代理身份/格式变化不能复用旧验证器，新 fetcher scope 也隔离同 ID 的配置或凭据变化。兼容旧 manifest v1 内容恢复，但首次刷新无条件获取；详见 [Resource](../../architecture/backend/modules/resource.md)。本地内容仍需读取/解析校验，不宣称消除编译成本。

## 完成事件与后台分发

[`ResolutionRuntime::start_with_metrics`](../../../backend/src/resolution.rs) 在进程级创建 ingress、cache commit 和详情投影队列。`ResolutionPublisher::try_publish` 无等待接收 `ResolutionEnvelope`，`run_dispatcher` 尝试分发 cache candidate、更新 stats、交给启用的 writer 聚合请求指标，再在启用详情时尝试入队；各项失败独立计数。

`run_cache_worker` 执行异步缓存 CAS，`run_detail_projector` 构造有界详情并提交 writer。这些任务句柄由 `ResolutionRuntime` 持有，随 service 关闭，不是请求线程中的 SQLite 或详情格式化操作。

service 在 core 返回时冻结 port 字段 `duration_millis` 和 `dns_core_duration_micros`：前者从 transport 接入计时点到 core 完成，后者仅 core 主链；都不包含响应编码/写回或后台排队、详情投影和数据库写入。DoH 总耗时可能包含入站 TLS 与 HTTP 读取/解析。dispatcher 的 `attempt_outcome` 维度也来自这一请求终态，不是独立的逐 upstream attempt 事件。

transport 捕获的可选原始 `client_id`/有效 client IP 随 `ResolutionDetailSource` 进入详情链。Policy 在当次 Runtime 内把 `ClientMatchObservation` 冻结为 `Id` 或 `Ip` 来源及匹配时的稳定客户端 ID；事件消费与后续 reload 不重新查询客户端目录。stats 的客户端维度只消费该稳定 ID，不读取可变管理名称；请求原始身份不进入 telemetry label 或事件 `Debug`。当前生产配置仍为 v1：仅单 ID 客户端的 IP 命中可无歧义生成稳定 ID，多 ID/IP 命中保持未知，待新配置基线接线后退出该过渡边界。

## Storage

[`StorageRuntime::open`](../../../backend/src/storage/service.rs) 在 DNS bind 前打开统计 SQLite，并构建独立的详情日分片 registry/writer；`database` 始终必需，关闭 `resolve_log` 不关闭聚合统计，也不会创建新的详情目录或文件。

[`StatsPersistenceWorker`](../../../backend/src/storage/stats.rs)、[`statistics.rs`](../../../backend/src/storage/statistics.rs) 与 [`ledger.rs`](../../../backend/src/storage/ledger.rs) 负责 epoch、待提交批次和幂等去重。SQLite adapter 在同一事务内更新聚合和 ledger，成功后 ack；普通不可用保留 pending 重试，pending 内存保护或不可恢复错误通过 service/Supervisor 处理。

详情由 [`resolve_log.rs`](../../../backend/src/storage/resolve_log.rs) 投影，再交 [`detail_shards.rs`](../../../backend/src/storage/detail_shards.rs) 的唯一有界 detail worker 批写。worker 按事件 UTC 日选择 `<records_path>/YYYY-MM-DD.sqlite3`，满批立即提交，低流量尾批由周期 flush 处理；一次事务只处理队首同日记录。registry 以日锁串行同一分片、全局最多 4 个活动单连接 pool，读 lease 缺文件时不建库，retirement 先阻止新 lease 再等待已有 lease。`writer.rs` 是内存 contract 实现，不是正式 SQLite writer。旧 [`SqliteManagementReadModel`](../../../backend/src/storage/management_read.rs) 仍读取主库，跨分片管理查询由 BC-09/13 替换。

[主库迁移目录](../../../backend/migrations)的前向链仍是 0001 基础表至 0007 client identity，统计库当前 schema 为 v7；其中旧 `resolve_log` 暂留兼容读口/测试，生产不再写入。新详情文件使用 [`migrations/detail`](../../../backend/migrations/detail) 的 layout v1：`detail_meta` 固定版本和唯一 UTC 日，`resolve_log` 保持当前有效字段，`(event_time_utc_millis, id)` 索引和 trigger 双重约束记录只能属于该文件日期。BC-08 不迁移、删除或重新匹配旧详情。两类 SQLite 均使用 WAL、NORMAL synchronous 和 busy timeout；统计库保留串行 operation lock，详情连接由 registry lease 管理。

主库升级由 adapter 手动执行 `include_str!` SQL 并更新 `storage_meta`，不是 SQLx Migrator。`connect_with_deadline` 将建目录、连接和迁移纳入 open 的同一预算；随后 `startup_write_probe` 在独立事务中实际更新 metadata 并回滚，不提交统计或详情。详情目录只在首个写 lease 时创建；已有日期文件必须声明匹配的 layout/day 和完整表、索引、trigger，普通外部 SQLite 或路径/文件身份异常会被拒绝。失败/超时不产生可服务 lease。

停机时 `run_until_stopped` 关闭详情输入并将剩余 worker/队列交回 owner；正在执行的批次先结束，其余详情不抢先排空。`StorageService::shutdown` 提交统计并关闭主 pool，随后 `StorageRuntime` 用剩余时间排空分片详情、拒绝新 lease 并等待活动连接归还。启动/停机 deadline 不重置，但不能强制中断已进入 OS/SQLite worker 的操作；超时不伪装为成功或零丢失。

### 业务时间存储

| 表 | 当前字段 | 类型与单位 |
| --- | --- | --- |
| `storage_meta` | `created_at_utc_millis`、`migrated_at_utc_millis` | `INTEGER`，Unix UTC 毫秒，非负 `i64` |
| `stats_batch_ledger` | `committed_at_utc_millis` | `INTEGER`，Unix UTC 毫秒，非负 `i64` |
| 主库旧 `resolve_log` / 日分片 `resolve_log` | `event_time_utc_millis` | `INTEGER`，Unix UTC 毫秒，非负 `i64`；生产只写日分片 |
| `stats_daily_total` / `stats_daily_dimension` | `day_utc` | `INTEGER`，epoch 起算的 UTC 自然日编号，语义不变 |
| 日分片 `detail_meta` | `day_utc`、`created_at_utc_millis` | 文件归属的 epoch UTC 日和 layout 创建时间 |
| 主库旧 `resolve_log` / 日分片 `resolve_log` | `duration_millis`、`dns_core_duration_micros` | `INTEGER` 耗时，分别为毫秒/微秒；旧历史主链耗时可为空 |

[`0006_integer_business_timestamps.sql`](../../../backend/migrations/0006_integer_business_timestamps.sql) 只迁移原四个绝对时间字段，不修改 0001–0005。它在同一事务中创建目标表、按完整字段复制、检查时间无损往返、替换表并重建 `(event_time_utc_millis, id)` 索引；stats 日表不重写，ledger hash/序号、详情 ID/其他字段/空值和 AUTOINCREMENT 历史高水位保留。最后才推进 schema version，并将 migrated time 更新为本次升级时间；重开和写探针不刷新该时间。

[`0007_client_identity.sql`](../../../backend/migrations/0007_client_identity.sql) 只为 `resolve_log` 增加可空 `client_id`、受限为 `id`/`ip` 的 `client_match_source` 和 `matched_client_id`。writer 要求匹配来源与匹配 ID 同时存在或同时缺失，并按配置 ID 长度规则校验两个 ID；旧行保持 null，不从 `client_bucket` 或 client IP 反推历史身份。

旧 writer 产生的规范非负十进制毫秒字符串可无损转换。空串、非数字、小数、指数格式、负值和超出 `i64` 的值不静默 `CAST` 成零或饱和值，迁移失败并回滚该步全部变更；不删除坏行或推测历史时间。新写入由 `system_time_utc_millis` 转为 `i64`，亚毫秒截断、epoch 前归零保留旧行为，溢出显式错误。时间列有 `typeof(...)='integer'` 与非负约束，不能保存不合法 TEXT/REAL 值。

分片 writer 用整数毫秒计算 UTC 日；文件内 trigger 再校验 `event_time_utc_millis / 86400000 == detail_meta.day_utc`。生产批写不执行历史 `COUNT`、按年龄/条数 `DELETE` 或 `VACUUM`，v1 三个详情配额字段只在 BC-26 删除前继续由旧 loader 解析校验。R/G/T 共同水位、manifest/ledger 和物理回收尚待 BC-10/11，当前不会自动删除分片。对外查询仍暂走旧读口，不能据此宣称跨日 API 已接线。

## Cache persistence

[`snapshot.rs`](../../../backend/src/cache/snapshot.rs) 实现独立 `FDCS` 完整快照格式。写入从生产 [`MokaCacheStore`](../../../backend/src/cache/moka.rs) 的弱一致视图按批次上限取得可见记录，批外编码并直接顺序写入同目录临时文件；不复制完整缓存或维护第二份 entry 集合。文件头记录独立版本、生成 UTC 毫秒、记录数、body 长度和覆盖 metadata/body 的 SHA-256，正式替换前 `sync_all`；Windows 使用 `MoveFileExW(REPLACE_EXISTING | WRITE_THROUGH)`，失败清理本轮临时文件并保留上一份快照。

`open_cache_snapshot` 在返回 reader 前先以调用方文件字节预算和 deadline 验证完整长度/摘要，随后 `CacheSnapshotReader` 分批解码；逐条复用现有 key/entry codec，按绝对 expiry/stale-until 扣除停机时间，隔离过期、损坏和不兼容记录。同一快照使用有界 SHA-256 key 集合去重，记录数上限为 100000、单条上限为 2 MiB；这些是内部恢复保护，不是磁盘配额或 Moka/RSS 一比一承诺。

[`CacheSnapshotOwner`](../../../backend/src/cache/snapshot_owner.rs) 是唯一进程级持有者。正式 app 在 Policy core 完成 prepare 后、listener bind 前把 `FDCS` 分批恢复到该 core 的 Moka；缺失、损坏、不兼容、超时或内存预算不足分别形成冷启/部分恢复状态，不阻止启动。恢复完成后 owner 启动一个周期 worker；内存 commit 不再产生逐条磁盘队列，Moka 仍是运行权威。

当前生产 loader 仍为 v1。BC-07 仅复用已解析的 `dns.cache.persistence.path` 作为快照路径，并使用内部固定 5 分钟周期；旧 `persistence.max_size_bytes` 不再参与生产快照。v2 的 `enabled/path/snapshot_interval` 正式加载与新数据基线初始化仍属于 P5 BC-26，不能从当前过渡接线推断已经完成 v2 启动。

[`RuntimeCoordinator`](../../../backend/src/runtime/coordinator.rs) 只登记一个 owner，并核对它与活动 revision/Moka source 一致。reload 在候选发布前校验新路径，Runtime CAS 成功后同步递增 generation 并切换 source；不从磁盘恢复候选，也不让旧写任务覆盖新代。发布前再次检查路径链接/文件身份及受保护文件 alias。shutdown 先排空历史和当前 [`LateCacheFinalizer`](../../../backend/src/cache/service.rs)，再在同一总 deadline 内 best-effort 写当前 Moka 的最终快照；Cache health 分别汇总 finalizer 与 snapshot gap，不记录 key、response、路径或底层原始错误。

旧 [`CachePersistenceRuntime`](../../../backend/src/cache/runtime.rs)、[`SqlitePersistentCacheStore`](../../../backend/src/cache/sqlite.rs)、文件 adapter 和确定性 [`MemoryCacheStore`](../../../backend/src/cache/memory.rs) 仍保留给既有契约测试及 P5 BC-27 删除工作，生产 `app/runtime/dns/service` 路径不再创建或挂接 SQLite cache persistence。统计主库与详情日分片均和缓存快照文件隔离，不受本次缓存切换影响。

Windows `_fluxdns/p2-cache-tests/`、`_fluxdns/p2-cache-owner-tests/` 和 `_fluxdns/p2-cache-owner-policy-tests/` 真实文件测试覆盖流式往返、停机 TTL、损坏/未知版本/文件预算、失败保留旧文件、周期跳过未变化代、预算缩小后的部分预热、reload/clear 代际仲裁与清理后不复活、路径 hard-link/alias、超时 shutdown，以及两个真实 `PolicyDnsCore` 之间的 `FDCS` 重启命中。后者直接核对文件头且确认没有 SQLite `-wal`/`-shm` sidecar；未执行真实权限/磁盘满、Unix 或个人配置启动。

BC-07 交付验证在 Windows、Rust/Cargo 1.98.0 执行：全量 `cargo test --locked -- --test-threads=4` 为 807 passed、0 failed、3 ignored；另以 `service::tests::`、`app::tests::`、`runtime::coordinator::tests::` 和跨 Policy core 重启用例定向核对接线。三个 ignored 仍是手动性能与 1024-session 专项。本轮未运行 Linux、真实磁盘故障、完整 v2 冷启/重启、浏览器或核心 2ms 性能验收。

## Observability

[`observability.rs`](../../../backend/src/observability.rs) 的 `TelemetryWriter`、`StructuredTelemetryOutput` 和 health registry 使用低基数、有界内存与安全 typed event；Application 在配置校验后切换正式日志目标和过滤器。P1 日志 owner 接线后，正式 app 始终创建 writer；`logs.enable` 只影响日志接纳及文件输出，不关闭指标和 health，见[日志热切换](#p1-日志热切换2026-09-07)。

[`service.rs`](../../../backend/src/service.rs) 为启用的 writer 创建 `TelemetrySampler`，在既有 5 秒周期 flush 前采样：

- 从同一个 `ResolutionPipelineMetrics` Arc 读取 accepted，将与共享游标的差值记录为 `ResolutionEventsAccepted`；仅成功后推进游标。重复采样、reload 与最终采样不会重复累计，源倒退/溢出明确报错。
- 将采样时的事件队列长度覆盖为 `WriterQueueDepth`。无 Resolution owner 只生成这一项；正式 app 的 `logs.enable=false` 仍保留同一个 writer/sampler 和周期任务，但不创建日志文件。

`TelemetryWriter` 持有 [registry.rs](../../../backend/src/observability/registry.rs) 的有界聚合器。周期采样保留上述两个 series，dispatcher 的 `record_resolution` 另更新固定 14 项：两个请求/core latency histogram、六种 outcome 和六种 cache status 计数。聚合共享 128 series 上限，不进入日志队列、不保存原始样本或按请求值生成标签。单事件更新先检查全部溢出，再整体发布；失败和关闭后 record 增加固定 `rejected_metrics`，不递归写故障事件。

周期 flush 处理开始时的有限事件批次，再输出最新聚合快照；输出中的 Counter/histogram 是 writer 实例内累计值、Gauge 是瞬时值。histogram 输出微秒 unit、count、sum 和固定累计桶，字段/精度见 [Observability](../../architecture/backend/modules/observability.md)。失败保留内存聚合，下次允许重复输出同一累计值，消费者不能把快照直接相加。快照和事件输出不持有 registry/state 锁，正在输出的事件预留容量，失败重新入队不突破事件上限。正式 health 只有 `TelemetryHealthRecord` 一套，旧 `EventWriter`/旧事件与重复 health 模型已删除。

最终 shutdown 在 Resolution、Cache finalizer、Storage 回收后再次采样，随后关闭 writer 输入、排空事件并输出最终累计值；所有步骤使用既有总预算。主输出失败可走 stderr fallback，双输出失败在进程内更新 health，完整 flush 成功可恢复状态。同步底层 Write 无强制中断保证；resolution ingress gap、详情丢弃、cache commit outcome 和数据库 persistence gap 也仍分别计量。

请求 instrumented core 和 Resolution publisher 不新增同步聚合调用；新指标只在后台 dispatcher 消费现有完成事件，`f732cd64` 的异步观测与缓存移交保持不变。`accepted` 仅代表 ingress 成功入队，histogram 则统计已消费并被指标接受的事件，都不是所有 DNS 请求或持久化成功数。详情或日志关闭不停止请求指标；测试可独立不注入 telemetry，这不等于正式 app 的 logs 开关。TypedTracingLayer 仍不建立完整 request/group/attempt span 树，本轮也不新增独立 attempt 事件流。

### P1 日志热切换（2026-09-07）

[`LoggingOwner`](../../../backend/src/observability/logging.rs) 复用既有 bootstrap filter handle、共享输出目标和 `TelemetryWriter`。app 即使从 `logs.enable=false` 启动也安装一次 typed tracing layer，将相同 writer 注入 Management、Storage/Resolution 和 service，再通过 `attach_logging` 校验 owner 与 writer/活动 logs 配置一致。不重复安装全局 subscriber，不因日志开关重置指标、health 或采样游标。

`DnsService::reload_prepared` 在绑定/发布前预开日志输出。文件打开移到单槽 `spawn_blocking`；超时放弃等待，但 worker 直到真实退出才释放槽位，不积压阻塞任务。仅变更 level 且 enable/path 不变时复用当前文件句柄；关闭日志不访问目标路径。预开可能创建空文件，不代表运行配置已应用；取消或失败不按路径删除日志文件。

发布前尝试取得 writer flush 和输出锁，Busy 时不等磁盘 flush、不执行 Runtime CAS。reload filter 失败不发布 Runtime；CAS 失败恢复旧 filter，输出目标仍是旧句柄，补偿失败使用独立 `LoggingCompensation` 返回，不能伪装为普通拒绝。CAS 成功后同步切换输出、直接 LogSink 的级别过滤及 owner 配置，再放行新服务任务；其中不再进行可失败的文件打开。关闭或收紧 level 会丢弃不再符合条件的排队日志，指标/health 保留；已接纳且仍符合新级别的排队日志允许写入新路径。

只有 service-aware 应用路径可切换日志。保留的 coordinator-only `reload_runtime_from_path` 在 prepare 前拒绝日志变化，没有挂接日志 owner 的 service 也拒绝变化；不能只更新 Runtime 的 logs 字段而不更新输出。文件 watcher 仍只提示，不因此恢复自动 reload。

Windows 定向证据：Observability 24 项通过，包含全局 subscriber 独立子进程、真实日志文件/Windows 占用失败、off/on、level/path、filter 失败和补偿失败区别；`service::` 筛选 70 项通过、3 项保持原有忽略标记，`app::` 13 项、`management::` 18 项通过。`cargo check`、全部测试目标 `--all-targets --no-run`、fmt 和文档检查通过；未改 schema、前端或依赖，本批未重跑前端验证。真实 UDP/SQLite service 联合测试通过连续五次日志切换和坏路径拒绝，DNS 持续查询，writer、sampler 和 Resolution metrics Source Arc 保持相同。测试目录为 `_fluxdns/p1-logging-tests/`、`_fluxdns/p1-logging-dns-tests/`；既有临时文件测试运行时将 TEMP/TMP 限定到 `_fluxdns/test-temp/`。

边界：正式 app 仍使用 v1 loader/runtime/storage，v2 配置事务生产者、应用后持久化联合路径和 HTTP/UI 日志保存未接线；上述证据不关闭完整 BC-31/CR-05 或 P1。filter/CAS 补偿失败的测试使用真实 reload handle 和故意撤销的测试 subscriber，不能视为生产 subscriber 曾失效。日志预开仅检查实际打开句柄为普通文件，尚未提供日志目标与其他受保护文件的完整物理 alias 防护，必须随 v2 写入安全边界闭合。未验证 OS 文件调用强制中断、Unix、日志轮转、磁盘满或性能；此次未改变 BC-23 的 QPS/RPM 口径，也未提供其新查询端点。

## 能力与证据

| 能力 | 代码实现 | 正式入口接线 | 验证证据 | 已知限制 |
| --- | --- | --- | --- | --- |
| remote/file 刷新 | 条件 fetch、manifest v2、epoch/CAS、scheduler | async prepare + service resource task | loopback 200/304 与真实条件头；重复 304、坏 pair/响应、旧 manifest、换代及同预算重试 | 未执行真实远程/代理组合 |
| stats/detail | 统计 schema v7、详情分片 layout v1、registry/lease、StorageRuntime、ResolutionRuntime | app 打开；统计主库与日分片 writer 分离 | 真实跨日/迟到分片、日归属、外部库拒绝、只读不建库、连接上限/退役/shutdown、小 v1 配额不截断；原统计迁移回归 | 跨分片读口、共同水位/回收待 BC-09 至 BC-11；未验证真实权限/磁盘满 |
| legacy cache persistence | schema v2、增量 upsert、CachePersistenceRuntime | 仅保留 adapter/契约测试，生产不再挂接 | v1 升级、增量触发器、失败回滚与坏行清理既有测试 | 待 P5 BC-27 删除；不代表当前生产路径 |
| cache 二进制快照 owner | `FDCS` header/SHA-256、Moka 分批导出/恢复、周期 worker、generation | app 启动恢复 + coordinator reload + service shutdown | Windows 真实文件、跨 Policy core 重启、周期/预算/损坏/alias/代际/超时定向测试 | 过渡期固定 5 分钟；未验证真实权限/磁盘满、Unix 或 v2 冷启 |
| telemetry lifecycle / 聚合 | histogram、typed writer、registry、sampler | dispatcher + app/service 周期及最终 flush | 固定桶/标签、溢出原子性、拥塞下聚合、关闭详情、输出重试、reload 与最终快照 | 没有 exporter/逐 attempt 流；长期负载与全部输出故障未验收 |

## 本次验证

本节保留时间整数迁移批次的既有结果。后续契约验证工作树的入口、证据类别和运行结果见[契约验证运行入口](#契约验证运行入口)，不要把两个工作树的计数或性能样本拼接为同一轮。

2026-09-08 P2 BC-08 验证在 Windows x86_64 使用 Rust/Cargo 1.98.0，真实 SQLite 文件位于仓库忽略的 `_fluxdns/p2-detail-shard-tests/` 或既有独立临时 `work.path`，用例结束仅清理各自唯一目录。`cargo test --manifest-path backend/Cargo.toml --locked storage:: -- --nocapture` 运行 75 项通过；完整 `cargo test --manifest-path backend/Cargo.toml --locked` 为 815 passed、0 failed、3 ignored，`--all-targets --no-run` 通过。覆盖两个 UTC 日和迟到记录、layout metadata/trigger、只读 lease 不建库、最多一个连接的受控场景、退役与关闭等待、外部 SQLite/硬链接拒绝，以及生产主库不新增详情和 v1 `max_records=3` 时 300 条分片记录全部提交。该结果不包含 Linux、真实权限/磁盘满、跨日 cursor 或保留删除。

2026-09-05 在 Windows x86_64 使用项目 mise 管理的 Rust/Cargo 1.98.0；命令从仓库根执行，`CARGO_HOME=backend/.cargo-home`、构建物在 `backend/target`。测试使用代码内嵌配置，临时 `work.path`、数据库与证书由测试夹具在 `_fluxdns/test-temp` 下产生，端口为动态 loopback，不使用个人配置或远程服务。

完整测试命令为 `cargo test --manifest-path backend/Cargo.toml --locked --quiet -- --test-threads=4`，运行前将 TEMP/TMP 指向 `_fluxdns/test-temp`；结果为 642 通过、0 失败、2 个手动 profile 默认忽略。缓存/Storage/资源夹具也使用 `_fluxdns/tests/`。本次新增时间转换边界、v5→v6 数据/ledger/自增序列保留与重开、删空后的高水位、四类异常时间回滚、新值 INTEGER 约束、数字排序/范围/清理和时间索引用例；v1 升级和实际 writer 类型读取同样回归通过。前次获批契约的异步主链、late-result、bootstrap、资源条件请求、指标与 panic 安全测试继续通过。

交付静态检查 `cargo check --manifest-path backend/Cargo.toml --locked`、`cargo fmt --manifest-path backend/Cargo.toml -- --check`、`pwsh -File .agents/skills/project-doc-maintenance/scripts/check-docs.ps1` 与 `git diff --check` 均通过；文档检查覆盖 42 份 Markdown、522 处链接与引用，不验证外链网络或产品语义。

本机性能对比入口为 `cargo test --manifest-path backend/Cargo.toml --locked service::tests::benchmark_udp_telemetry_sampling_profile -- --ignored --nocapture`。它使用 debug 构建、4 个 runtime worker、单 UDP client，每秒一批 1,000 次 hosts 查询，至少覆盖两个 5 秒采样周期；保留真实 Resolution/SQLite 后台任务，JSON 输出写 sink，不测磁盘日志吞吐。延迟只统计实际请求，不含批次间等待，输出的 sequential_qps 不能视为整段负载吞吐。

下列手动 profile 是本次时间迁移前、已提交 `43671f1` 的单轮样本；本轮未重跑两个手动 profile，不从旧样本推断 v6 迁移成本、发布性能提升或稳定回归百分比：

| 模式 | 计时样本 | 平均 / p50 / p95 / p99（ms） | 最终资源状态 |
| --- | --- | --- | --- |
| 关闭 telemetry | 12,000 | 0.143020 / 0.126200 / 0.255300 / 0.322900 | source accepted 12,100（含预热），0 series，无 telemetry 任务 |
| 开启 telemetry | 12,000 | 0.153684 / 0.133200 / 0.255100 / 0.331400 | 同样 accepted 12,100，16 series，最终队列 0，输出 77 项，rejected_metrics 0 |

本次不修改 Storage 积压预算，不把短时 profile 外推为稳定压力通过；积压保护与长期恢复的证据边界见[验证范围与收口](#验证范围与收口)。真实远程、真实磁盘满/权限/介质故障、Unix 信号、发布硬件 SLO 和长期 RSS/CPU 压测均未执行；SQLite 写锁和 trigger 故障只证明已列出的本地分支。

## 契约验证运行入口

[`script/test-backend-contracts.ps1`](../../../script/test-backend-contracts.ps1) 是显式本机验证与证据收集入口，不安装工具、不改变用户配置，不运行真实磁盘故障、远程请求、Unix 信号或长期性能实验。从仓库根目录执行：

```powershell
pwsh -File script/test-backend-contracts.ps1 -Suite Local -Repeat 3
pwsh -File script/test-backend-contracts.ps1 -Suite Connections -Repeat 3
```

`Local` 先执行 fmt/check，再重复 `contract_v` 定向用例，最后执行默认全量回归；`Connections` 只显式选择 V6-C01。两种入口不能互相替代，默认全量测试仍忽略容量实验与两个手动 profile。`-Repeat` 为 1–20 次，`-TimeoutSeconds` 为每个子命令 watchdog（默认 300 秒）；测试内部保留各自业务 deadline 和更短 watchdog。

输出统一写入 `_fluxdns/contract-validation/<UTC-run-id>/`：每条命令有 stdout/stderr，`report.json` 保存模式、命令、实际可执行文件、Rust/Cargo、OS、源码 HEAD、含未提交源码的前后指纹、lockfile hash、elapsed、退出码及 watchdog kill。测试 TEMP/TMP/TMPDIR 固定到 `_fluxdns/test-temp`。非零退出、watchdog kill、空测试筛选或运行期间源码变化均失败，不生成虚假通过记录；中间失败日志保留。连接、task 与数据库清理由用例断言，原始数据库夹具允许留在忽略目录，不声称脚本已逐个删除文件。

### Storage 用例

| 用例 | 前置与故障点 | 断言与边界 |
| --- | --- | --- |
| V4-M01 | `sqlite::tests::contract_v4_all_legacy_versions_preserve_rows_and_nulls_on_reopen`；用原 migration 构造 v1–v5，各自空库/含数据 | 每个起点升级后两次打开，对照 metadata、全部详情字段/历史 NULL、统计总数/维度、ledger hash/序号、删除后的自增高水位和 integrity check；不重写旧 migration |
| V4-M02 | `contract_v4_each_migration_failure_preserves_last_committed_step`；从 v1 出发，分别让 v2–v5 metadata update trigger 失败及 v6 时间转换失败；v7 从真实 v6 起点让 metadata update trigger 失败 | schema version 和已提交字段保留在失败前一步，该步新增列不残留；解除故障后可升级。v1 建库失败另复用原 DDL 回滚测试 |
| V4-M03 | `contract_v4_newer_schema_is_rejected_without_mutation` | v8 被拒绝，schema SQL 与版本保持原样；不自动降级 |
| V4-S01 / V9-S-local | `contract_v4_midnight_late_events_and_repeated_sqlite_recovery`；真实 SQLite，三轮事务 trigger 失败/解除/重试 | `day_utc` 从午夜两侧事件时间计算日桶；乱序与 late event 分属两个 epoch，失败不写 ledger，恢复后无重复总数，pending/gap 清除，重开及 integrity check 通过。trigger 不等价 disk-full 或介质 I/O 故障 |
| V4-S02 | `stats::tests::contract_v4_pending_event_limit_preserves_active_epoch`；内存 backend 拒绝提交 | 分别达到 65,535/65,536 pending events，再产生两条 active event，保护错误保留 pending 与 active；batch 数上限继续复用原 64-batch 用例 |
| V4-S03 | `storage::service::tests::contract_v4_sql_stages_share_shutdown_budget_and_reclaim_owner`；正式 StorageRuntime、真实日分片 SQLite，SQL 前/已 INSERT 未提交/已提交待回收 × 放行/截止超时 | 当前详情事务先回收，owner 尚未进入统计提交；正常释放后 stats/detail 均完成，超时报告失败且不延长预算。未提交详情回滚、已提交详情保留，pending 统计可在显式新预算下幂等恢复；主库无详情新行、channel/句柄/lease 回收及 integrity check 均断言 |
| P2-S08 | `detail_shards::tests` 与 `storage_runtime_separates_stats_and_ignores_v1_detail_record_limits`；真实 UTC 日文件、迟到/错误日、缺失读取、外部库、连接和退役交错 | 文件名和 metadata 日一致，错误日由 trigger 回滚；只读不创建、连接受全局上限、shutdown 等待 lease；生产主库详情保持空，小 v1 条数/年龄配置不触发分片 COUNT/DELETE 配额路径 |
| V4-S04 | `stats::tests::contract_v4_concurrent_flush_wait_preserves_deadline_and_active_epoch`；显式持有上一轮 flush 的串行锁 | 新调用的 20ms 预算耗尽即返回 Timeout，未提前交换 active epoch；放锁后可且仅可提交一次 |

历史 v5 的完整非 NULL 字段、异常时间/INTEGER 约束、自增删除高水位及时间排序/索引用例继续复用。主库 SQLite operation lock/pool wait、同批幂等提交和旧单库详情配额测试仍作为兼容 adapter 回归，不能代表生产分片仍按条数/年龄清理。分片满批/尾批和 stats-first shutdown 由 P2-S08/V4-S03 覆盖。V4-S03 的暂停点仅在 `cfg(test)` 的单个分片 store 实例启用，分别位于真实 SQL 前、commit 前和 commit 后；不把同步点或 Tokio future 取消描述为可强制抢占 SQLite 系统调用。

V4-S04 在修复前等待至 200ms watchdog，而没有在 20ms 调用预算内结束。[`StatsPersistenceWorker::flush`](../../../backend/src/storage/stats.rs) 现对 `flush_lock` 的排队使用原 deadline，超时返回 `Timeout / stats_persistence.flush_lock`；拿锁后的 epoch、pending、ledger 与提交顺序保持不变，没有增加重试、持久化主链或内存上限。

### 环境与结果边界

2026-09-05 的实施环境为 Windows x86_64，mise 解析 Rust 1.98.0，当时 HEAD 为 `f65fb3f8bd68e1a40ca041d9a380859b44a3da0c`，验证包含该批未提交工作树；最终源码指纹和实际命令以本地报告为准。使用合成配置、动态 loopback 与本地 SQLite，不读取个人配置、凭据、共享数据库或任意公网端点。

2026-09-05（UTC）的 Local runner 实际结果：27 项新增默认测试重复 3 次均通过；完整回归 **669 通过、0 失败、3 忽略**（两个既有手动 profile 和新 V6-C01）。`cargo fmt --check` 与 `cargo check --locked` 同轮通过。Connections 模式另行重复 3 次通过，两个协议每轮均记录 accepted 1,023/1,024、超额等待 1、恢复 1、停机请求数 0 和端口重绑成功；这是独立重复运行，不是连续长期容量结论。

本轮原始输出位于 `_fluxdns/contract-validation/20260905T161832928Z-43588/`（Local）及 `20260905T162144439Z-28988/`（Connections）。两个报告的源码前后 SHA-256 指纹均为 `5C3A2860385D2541F66B2C1EFE77CAC08C46F31039BC45235D835CA8A6E5F838`；源码未在两组验证间变化。修复前的复现错误与最小修复分别记录于本节和[生命周期](lifecycle.md#契约验证补充)，不拼接不同源码状态宣称最终通过。

### 验证范围与收口

2026-09-06 经用户确认，结束后端契约验证专项的剩余测试工具开发与环境验收，移除活动计划及索引，不归档、不保留跳转副本。尚未实施的故障/远程/Unix 专用编排、联合恢复对账、OS 资源采样和长期性能对照不再作为本专项的活动待办；它们仍是未实现或未验收范围，不能标为完成或通过。今后需要重新开展时，按新的目标和授权另行确定范围，不因本次收口自动执行。

已实现的生产修复、现有测试和 `contract-load` 工具保留，入口与实际证据继续由本页、[DNS 管线](dns-pipeline.md#契约验证补充)及[生命周期](lifecycle.md#契约验证补充)维护。配置语义、异步响应主链、owner/失败升级策略、共享 deadline 与资源预算不变；body 断流维持现有 `Internal`/不可重试行为，具体限制见[Adapter 支持矩阵](dns-pipeline.md#adapter-支持矩阵)，不再保留分类调整待办。

下表集中记录未验收边界。“尚未指定/核验环境”不代表断言用户没有环境；结束专项也不等于取得目标环境证据。本次未安装代理、驱动或工具，后续任何实际实验仍遵守[环境规范](../../rules/environment-usage.md)和[本地测试规范](../../rules/local-testing.md)中的工具、授权及本地数据边界。

| 未验收范围 | 当前证据不能证明什么 | 实现与证据入口 |
| --- | --- | --- |
| 网络分支 | 已 poll read/握手取消不证明 OS connect 挂起取消或跨 OS 终态；截断 body 的 Internal/不可重试按现状保留 | [Adapter 支持矩阵](dns-pipeline.md#adapter-支持矩阵) |
| 真实介质故障 | SQLite trigger/锁不证明 disk-full、权限拒绝或介质 I/O 失败；没有各故障类型的显式驱动和获准介质记录 | [Storage 用例](#storage-用例)、[Cache persistence](#cache-persistence) |
| 连接与代理 | 已补连续重连/reload/rebind 与慢 body/畸形连接；不证明 OS 句柄趋势、TLS 混合容量、external TLS 终止链或真实出站 SOCKS | [真实会话边界](dns-pipeline.md#真实会话边界) |
| 远程资源 | loopback 条件请求不证明真实远程端点的 validator、pair/代际与刷新/reload 交错；没有可控目标和专用编排驱动 | [资源准备与刷新](#资源准备与刷新) |
| Unix 信号 | Windows/内部取消不能证明真实 PID 的首/第二信号、预算耗尽退出和端口/数据库终态；没有真实信号编排驱动 | [Shutdown 与错误](lifecycle.md#shutdown-与错误) |
| 持续积压恢复 | 已有多阶段/多周期流量入口；三个 trigger 周期或流量阶段切换不证明真实故障下的长期容量保护与 Cache/连接/owner 联合恢复 | [Storage 用例](#storage-用例)、[跨平台负载驱动](#跨平台负载驱动) |
| 长期负载 | 已有跨平台 Rust 发送与有界报告入口；短时 smoke 不证明 release SLO 或 RSS/句柄长期稳定，对照编排、资源采样和阈值未冻结 | [跨平台负载驱动](#跨平台负载驱动)、[本次开发验证](#本次开发验证) |

### 跨平台负载驱动

[`contract-load`](../../../backend/examples/contract-load/main.rs) 是独立 Cargo example，可直接编译为 Windows、Linux、macOS 等目标支持的可执行文件。它只使用现有 Tokio、Reqwest/Rustls、Hickory、Serde 和 SHA-256 依赖，不改变主程序 CLI、生产 YAML、发布 workflow 或默认二进制集合；运行编译后的文件不要求安装 Rust 或 PowerShell。没有新增 OS 专属调用，实际编译平台仍以本次记录为准。

从仓库根目录构建；跨目标构建须事先具备对应 target/linker，不自动安装：

```powershell
cargo build --manifest-path backend/Cargo.toml --locked --release --example contract-load
cargo build --manifest-path backend/Cargo.toml --locked --release --example contract-load --target <target-triple>
```

本机产物为 `backend/target/release/examples/contract-load[.exe]`；显式 target 的产物多一层 `<target-triple>/`。命令为 `contract-load CONFIG.json [REPORT_ROOT]`，报告根目录默认 `_fluxdns/contract-validation/`。每次创建独立目录，已有文件不覆盖，不自动启动/终止服务、修改配置、施加故障或触发 reload。

合成 [service.yaml](../../../backend/examples/contract-load/service.yaml) 和 [smoke.json](../../../backend/examples/contract-load/smoke.json) 使用 loopback 端口 15353/18080、合成 hosts 和根目录 `_fluxdns/contract-load-service/`；它们不是个人配置或服务默认配置。先核对端口空闲，在单独终端启动合成服务，再显式运行驱动；结束后关闭本次服务：

```powershell
cargo run --manifest-path backend/Cargo.toml --locked --bin fluxdns -- run --config backend/examples/contract-load/service.yaml
cargo run --manifest-path backend/Cargo.toml --locked --example contract-load -- backend/examples/contract-load/smoke.json
```

配置由 [`config.rs`](../../../backend/examples/contract-load/config.rs) 严格反序列化，未知字段/缺失参数拒绝，未提供隐式长时负载。自定义配置和原始结果保持在 `_fluxdns/`：

| 字段 | 行为与边界 |
| --- | --- |
| `targets` | 1–16 个不重复 ASCII `alias`，协议为 `udp`、`tcp`、`doh-get`、`doh-post`；UDP/TCP 要求显式 IP:port，DoH 要求 HTTP/HTTPS URL，不支持 userinfo、预置 query、fragment |
| `queries` | 1–4,096 条固定 `name`、`record_type`、`expected_rcode`、`min_answers`；轮流跨 target/query 组合，不记录原始域名/地址 |
| `phases` / `cycles` | phase 显式给 `alias`、`duration_ms`、`qps`；1–64 个阶段重复 1–100 轮，总发送时长最多 24 小时，QPS 是所有 target 的总量 |
| `concurrency` / `reuse_connections` | 并发与 target 数乘积最多 4,096；TCP/UDP 在 worker 内按 target 复用，DoH 复用有界空闲池；关闭复用时每次使用新连接 |
| `timeout_ms` | 1–60,000ms；拨号、TLS、写入、读取共用同一次 timeout，错误/超时后的 TCP/UDP session 丢弃，无额外重试 |
| `sample_interval_ms` | 100–60,000ms，输出累计完成数、错误、在途数、漏发数和成功延迟；桶/汇总内存有界，不保存逐请求样本 |
| `max_errors` / `max_scheduler_lag_ms` | 失败数超过上限停止新增请求；发送调度滞后超过显式 1–60,000ms 上限也停止；已有请求只等待原预算回收 |
| `seed` | 固定 query 起点和 DNS ID 序列，不代表随机工作负载 |

发送采用 open-loop 定时，不追赶历史 tick。过期 tick 计入 `skipped_schedule`，无空闲 worker 计入 `skipped_capacity`；不能把发送侧过载隐藏为较低的目标 QPS。阶段切换不重置在途请求预算，配置的协议比例为 target 等频轮转，负答案须在查询中显式给定期望 RCODE。

[`exchange.rs`](../../../backend/examples/contract-load/exchange.rs) 使用真实 UDP/TCP/HTTP I/O 和 Hickory wire 校验。DoH 禁用系统代理和重定向，保持 TLS 身份校验，使用 HTTP/1.x；响应要求 HTTP 200、正确 media type、最多 65,535 字节。所有协议检查 QR、opcode、ID、question、RCODE、最低答案数、完整 wire，拒绝 TC 截断，不自动发 TCP fallback。只校验这些字段，不证明答案地址逐项正确或缓存/Storage 已对账。

[`report.rs`](../../../backend/examples/contract-load/report.rs) 输出 `samples.jsonl` 和最终 `report.json`，保留 OS/架构、debug 标记、driver/编译时 Cargo.lock/配置 SHA-256、阶段、参数、分协议结果、错误类别/OS code 和清理状态；不输出目标 URL、qname、原始 wire。延迟 p50/p95/p99 只包含成功请求，是固定对数桶的微秒上界；没有成功样本时为 `null`，失败/超时另行计数。`completed_qps` 包含末尾请求回收时间，不能将其当作阶段瞬时 QPS。

Ctrl-C 停止发送并有界回收。退出码 0 表示测量完成且有成功样本、无请求失败/漏发/未归账项；1 表示失败、漏发或提前停止；2 表示配置、I/O 或驱动错误。进程被强制杀死时可能只有 JSONL，不能认为已有完整结果。所有报告固定为 `measurement_only`；CPU/RSS/句柄/磁盘趋势、目标二进制/配置指纹、故障控制、数据对账、候选/对照自动编排仍由外部场景提供，不能仅凭驱动退出 0 关闭 V9/V10。

### 本次开发验证

2026-09-06 按用户调整，仅将可本机执行的编译、轻量驱动验证与文档检查作为本次代码交付检查；不强制新增单元测试或运行全量回归。环境缺口不阻塞本次开发，但仍不称为验收通过。

已执行合成 loopback 的 UDP、TCP、DoH GET/POST 两轮稳态/增压/恢复短时测量：共 160 次请求，每协议 40 次，全部成功，错误、调度漏发、并发漏发和未归账项均为 0，驱动清理完成。原始结果位于 `_fluxdns/contract-validation/load-1788667044207-61892/`；这是 debug 驱动正确性 smoke，不是发布性能结论。合成服务只由本次验证进程创建并在结束后清理，未使用个人配置或远程目标。

最终 Windows x86_64 release binary 再次运行四协议短时检查：连接复用模式 160/160 成功，禁用复用模式 80/80 成功，无漏发/未归账项；故意不匹配的 RCODE 产生 `dns_answer`、`error_limit` 和退出码 1，空目标配置在发送前返回退出码 2。三个报告目录分别为 `_fluxdns/contract-validation/load-1788667857317-37600/`、`load-1788667863443-24896/`、`load-1788667866512-11436/`；所有已启动请求均完成回收。正常测量和预期失败分别记录，不将错误场景的非零退出码当作驱动异常。

扩展后的 Connections 入口以 `-Repeat 3 -TimeoutSeconds 180` 重复三次通过，每次包含 TCP/plain DoH 各三轮连续容量恢复、失败 reload、成功 reuse/rebind 和坏/慢连接子集。原始命令、前后源码指纹及结果位于 `_fluxdns/contract-validation/20260906T035742850Z-54984/`。初次扩展曾在第三轮等待新 session 超时；最终夹具增加 EOF/reset 终态等待，不把 request guard 归零直接作为下一轮的会话释放证据；未据此修改生产 listener。

本次实际执行的静态/构建检查：

| 检查 | 结果与范围 |
| --- | --- |
| `cargo fmt --manifest-path backend/Cargo.toml -- --check` | 通过 |
| `cargo check --manifest-path backend/Cargo.toml --locked --all-targets` | 通过，包含主程序、既有测试和新增 example 的编译检查 |
| `cargo clippy --manifest-path backend/Cargo.toml --locked --bin fluxdns --example contract-load -- -D warnings` | 通过，主程序与独立负载驱动 |
| `cargo clippy --manifest-path backend/Cargo.toml --locked --all-targets -- -D warnings` | 未通过：既有 `storage/sqlite.rs` 的 `DetailSqlTestStage`/`TestGate` 字段触发 `clippy::type_complexity`；本轮未改该文件，不顺手重构 |
| `cargo build --manifest-path backend/Cargo.toml --locked --release --bin fluxdns --example contract-load` | 通过，生成 `backend/target/release/fluxdns.exe` 和 `backend/target/release/examples/contract-load.exe` |
| 文档检查器与 `git diff --check` | 通过，未验证外链或目标环境语义 |

本机已安装的 target 只有 `x86_64-pc-windows-msvc`，本次仅实际编译该目标；未安装其他 target/linker，也未宣称 Linux/macOS 交叉编译、Unix 进程信号、真实代理/介质故障或长期性能验收通过。默认全量单元测试未重跑，没有新增单元测试框架或扩大默认测试集合；生产配置、Cargo.lock、依赖和发布矩阵未修改。
