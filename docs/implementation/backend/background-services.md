# 后台服务实现

> 文档状态：有效
>
> 适用范围：资源刷新、解析完成事件、统计/详情、缓存持久化和观测的实际所有权
>
> 最后核对：2026-09-05（获批 D1-D9 的后台契约接线及本地验证）
>
> 核对基线：`19c3c81e4fdbea9424d522620ad81462c6d22eb1` 加本次后端契约实施工作树

## 资源准备与刷新

[`PreparedRuntime::prepare_with_policy_core_and_remote_resources`](../../../backend/src/runtime/prepared.rs) 装配 [`ReqwestResourceFetcher`](../../../backend/src/resource/fetcher.rs)，先形成 file hosts/rule-set 和 remote rule-set 的可用 snapshot，再创建 core。remote 数据由 [`remote.rs`](../../../backend/src/resource/remote.rs) 恢复或抓取并持久化，解析器在 [`resource/rules.rs`](../../../backend/src/resource/rules.rs) 与 [`hosts.rs`](../../../backend/src/resource/hosts.rs)，不是请求时解析原始文件。

`auto_update` 对应的 worker 由 [`service.rs`](../../../backend/src/service.rs) 的 `run_resource_refresh_loop` 纳入 Supervisor，通过 coordinator 查询当前活动实例。刷新在 per-resource epoch/CAS 边界发布，并合并 Policy 内容与 runtime metadata；旧候选/旧 runtime 结果不会直接覆盖新实例。失败保留旧 snapshot，stale/retry 由 [`scheduler.rs`](../../../backend/src/resource/scheduler.rs) 与 [`refresh.rs`](../../../backend/src/resource/refresh.rs) 管理。

Policy 的 matcher/version/hash 先一起发布，随后单独更新 Runtime metadata，不是跨对象原子事务。fetcher 支持有界 ETag/Last-Modified 条件请求，结果区分 `Modified`/`NotModified`；拒绝重定向和其余非成功状态。2xx body 有界读入内存，解析成功才保存 content/manifest。

manifest v2 保存源身份 digest、fetcher 代际及不透明验证器，不保存原始 URL。只有本地 pair 完整、身份及代际匹配才发送条件头；304 返回后再次检查 pair，更新 manifest 与成功状态，不重写 body。失去条件基础时最多补发一次同 deadline 的无条件请求，再次 304 明确失败。URL/代理身份/格式变化不能复用旧验证器，新 fetcher scope 也隔离同 ID 的配置或凭据变化。兼容旧 manifest v1 内容恢复，但首次刷新无条件获取；详见 [Resource](../../architecture/backend/modules/resource.md)。本地内容仍需读取/解析校验，不宣称消除编译成本。

## 完成事件与后台分发

[`ResolutionRuntime::start_with_metrics`](../../../backend/src/resolution.rs) 在进程级创建 ingress、cache commit 和详情投影队列。`ResolutionPublisher::try_publish` 无等待接收 `ResolutionEnvelope`，`run_dispatcher` 尝试分发 cache candidate、更新 stats、交给启用的 writer 聚合请求指标，再在启用详情时尝试入队；各项失败独立计数。

`run_cache_worker` 执行异步缓存 CAS，`run_detail_projector` 构造有界详情并提交 writer。这些任务句柄由 `ResolutionRuntime` 持有，随 service 关闭，不是请求线程中的 SQLite 或详情格式化操作。

service 在 core 返回时冻结 port 字段 `duration_millis` 和 `dns_core_duration_micros`：前者从 transport 接入计时点到 core 完成，后者仅 core 主链；都不包含响应编码/写回或后台排队、详情投影和数据库写入。DoH 总耗时可能包含入站 TLS 与 HTTP 读取/解析。dispatcher 的 `attempt_outcome` 维度也来自这一请求终态，不是独立的逐 upstream attempt 事件。

## Storage

[`StorageRuntime::open`](../../../backend/src/storage/service.rs) 在 DNS bind 前打开 SQLite 并构建 stats/detail 能力；`database` 始终必需，关闭 `resolve_log` 不关闭聚合统计。

[`StatsPersistenceWorker`](../../../backend/src/storage/stats.rs)、[`statistics.rs`](../../../backend/src/storage/statistics.rs) 与 [`ledger.rs`](../../../backend/src/storage/ledger.rs) 负责 epoch、待提交批次和幂等去重。SQLite adapter 在同一事务内更新聚合和 ledger，成功后 ack；普通不可用保留 pending 重试，pending 内存保护或不可恢复错误通过 service/Supervisor 处理。

详情由 [`resolve_log.rs`](../../../backend/src/storage/resolve_log.rs) 投影，再交 [`sqlite.rs`](../../../backend/src/storage/sqlite.rs) 的唯一有界 detail worker 批写，满批立即提交，低流量尾批由周期 flush 处理；`writer.rs` 是内存 contract 实现，不是正式 SQLite writer。管理查询使用 [`SqliteManagementReadModel`](../../../backend/src/storage/management_read.rs) 独立只读 pool，不复用请求写入链路。

[迁移目录](../../../backend/migrations)的前向链是 0001 基础表、0002 resolution metadata、0003 management query projection、0004 query record observability、0005 DNS core duration。v5 前的主链耗时为 null；v4 前的脱敏详情标记为 legacy_redacted，不回填丢失内容。新库也执行同一链。SQLite 使用 WAL、NORMAL synchronous、busy timeout 和串行 operation lock；内存 adapter 是契约基线，不替代正式数据库。

升级由 adapter 手动执行 `include_str!` SQL 并更新 `storage_meta`，不是 SQLx Migrator。`connect_with_deadline` 将建目录、连接和迁移纳入 open 的同一预算；随后 `startup_write_probe` 在独立事务中实际更新 metadata 并回滚，不提交统计或详情。失败/超时不产生可服务 owner。旧业务 schema 的 metadata 时间仍为 Unix 毫秒字符串，本轮不变更业务时间类型。

停机时 `run_until_stopped` 关闭详情输入并将剩余 worker/队列交回 owner；正在执行的批次先结束，其余详情不抢先排空。`StorageService::shutdown` 先提交统计，再用剩余时间排空详情，最后关闭 pool。启动/停机 deadline 不重置，但不能强制中断已进入 OS/SQLite worker 的操作；超时不伪装为成功或零丢失。

## Cache persistence

[`PolicyDnsCore::initialize_cache_persistence`](../../../backend/src/dns/policy.rs) 在 async prepare 中打开独立 [`SqlitePersistentCacheStore`](../../../backend/src/cache/sqlite.rs)，恢复可用 entry 到 Moka。同步/测试构造器不因此自动产生磁盘副作用。

[`CachePersistenceRuntime`](../../../backend/src/cache/runtime.rs) 通过有界队列接收成功内存 commit 的持久化批次；单 writer 串行 I/O，失败 best-effort 计数，不令 DNS 响应失败。`recover/persist/maintain_capacity/shutdown` 共用 operation lock 和调用者 deadline，恢复检查 format、checksum、expiry 与 key compatibility。容量约束的是编码快照字节数，没有 SQLite page budget/文件物理硬上限，见[配置参考](../configuration.md)。

[`persistence.rs`](../../../backend/src/cache/persistence.rs) 的文件 adapter/codec 和 [`memory.rs`](../../../backend/src/cache/memory.rs) 用于替代实现与契约测试；正式默认仍是 Moka + SQLite。SQLite schema v2 在旧 payload 之外保存完整 key 唯一索引、编码大小及时间/排序索引；v1 一次性事务迁移保留有效内容和 row ID。常规批写只 upsert 变更项，不解码或重写其余 payload；过期/已发现坏行清理、容量裁剪同属一个事务，失败回滚。

容量合计仍扫描轻量索引，不宣称整个批次 O(变更项)。计费继续为 10 字节头加每条 payload 与 4 字节 framing，按插入时间淘汰而非访问热度。插入/可见截止时间索引为 Unix 纳秒整数，避免无关 key 更新重置旧项年龄。详见 [Cache](../../architecture/backend/modules/cache.md)。

coordinator 保留历史与当前 [`LateCacheFinalizer`](../../../backend/src/cache/service.rs) owner，shutdown 在同一 deadline 排空并汇总 persistence success/failure/drop。关闭 telemetry 前发布安全计数与 Cache health/gap，不记录 key、response 或 adapter 原始错误。

## Observability

[`observability.rs`](../../../backend/src/observability.rs) 的 `TelemetryWriter`、`StructuredTelemetryOutput` 和 health registry 使用低基数、有界内存与安全 typed event；Application 在配置校验后切换正式日志目标和过滤器。`logs.enable` 影响运行 telemetry 的创建，不应从设计存在推断任意配置都启用全部观测。

[`service.rs`](../../../backend/src/service.rs) 为启用的 writer 创建 `TelemetrySampler`，在既有 5 秒周期 flush 前采样：

- 从同一个 `ResolutionPipelineMetrics` Arc 读取 accepted，将与共享游标的差值记录为 `ResolutionEventsAccepted`；仅成功后推进游标。重复采样、reload 与最终采样不会重复累计，源倒退/溢出明确报错。
- 将采样时的事件队列长度覆盖为 `WriterQueueDepth`。无 Resolution owner 只生成这一项；`logs.enable=false` 不构造运行 writer、sampler 或额外任务。

`TelemetryWriter` 持有 [registry.rs](../../../backend/src/observability/registry.rs) 的有界聚合器。周期采样保留上述两个 series，dispatcher 的 `record_resolution` 另更新固定 14 项：两个请求/core latency histogram、六种 outcome 和六种 cache status 计数。聚合共享 128 series 上限，不进入日志队列、不保存原始样本或按请求值生成标签。单事件更新先检查全部溢出，再整体发布；失败和关闭后 record 增加固定 `rejected_metrics`，不递归写故障事件。

周期 flush 处理开始时的有限事件批次，再输出最新聚合快照；输出中的 Counter/histogram 是 writer 实例内累计值、Gauge 是瞬时值。histogram 输出微秒 unit、count、sum 和固定累计桶，字段/精度见 [Observability](../../architecture/backend/modules/observability.md)。失败保留内存聚合，下次允许重复输出同一累计值，消费者不能把快照直接相加。快照和事件输出不持有 registry/state 锁，正在输出的事件预留容量，失败重新入队不突破事件上限。正式 health 只有 `TelemetryHealthRecord` 一套，旧 `EventWriter`/旧事件与重复 health 模型已删除。

最终 shutdown 在 Resolution、Cache finalizer、Storage 回收后再次采样，随后关闭 writer 输入、排空事件并输出最终累计值；所有步骤使用既有总预算。主输出失败可走 stderr fallback，双输出失败在进程内更新 health，完整 flush 成功可恢复状态。同步底层 Write 无强制中断保证；resolution ingress gap、详情丢弃、cache commit outcome 和数据库 persistence gap 也仍分别计量。

请求 instrumented core 和 Resolution publisher 不新增同步聚合调用；新指标只在后台 dispatcher 消费现有完成事件，`f732cd64` 的异步观测与缓存移交保持不变。`accepted` 仅代表 ingress 成功入队，histogram 则统计已消费并被指标接受的事件，都不是所有 DNS 请求或持久化成功数。详情关闭不停止请求指标；telemetry 关闭不停止 stats。TypedTracingLayer 仍不建立完整 request/group/attempt span 树，本轮也不新增独立 attempt 事件流。
## 能力与证据

| 能力 | 代码实现 | 正式入口接线 | 验证证据 | 已知限制 |
| --- | --- | --- | --- | --- |
| remote/file 刷新 | 条件 fetch、manifest v2、epoch/CAS、scheduler | async prepare + service resource task | loopback 200/304 与真实条件头；重复 304、坏 pair/响应、旧 manifest、换代及同预算重试 | 未执行真实远程/代理组合 |
| stats/detail | 启动 deadline/probe、StorageRuntime、ResolutionRuntime | app 打开，service 持有并复用 sink；stats-first shutdown | 真实 SQLite 锁/写入拒绝/回滚；trigger 验证统计先于 300 条多批详情 | ingress/pending/数据库故障仍可产生明确 gap |
| cache 恢复/后台写 | schema v2、增量 upsert、CachePersistenceRuntime | core prepare + commit worker + finalizer shutdown | v1 升级保留 payload/重复 key；trigger 证明仅改动行写入；失败回滚、坏行清理、原 Busy/DiskFull 用例 | 注入不等价真实 disk-full；保留插入时间淘汰 |
| telemetry lifecycle / 聚合 | histogram、typed writer、registry、sampler | dispatcher + app/service 周期及最终 flush | 固定桶/标签、溢出原子性、拥塞下聚合、关闭详情、输出重试、reload 与最终快照 | 没有 exporter/逐 attempt 流；长期负载与全部输出故障未验收 |

## 本次验证

2026-09-05 在 Windows x86_64 使用项目 mise 管理的 Rust/Cargo 1.98.0；命令从仓库根执行，`CARGO_HOME=backend/.cargo-home`、构建物在 `backend/target`。测试使用代码内嵌配置，临时 `work.path`、数据库与证书由测试夹具在 `_fluxdns/test-temp` 下产生，端口为动态 loopback，不使用个人配置或远程服务。

完整测试命令为 `cargo test --manifest-path backend/Cargo.toml --locked --quiet -- --test-threads=4`，运行前将 TEMP/TMP 指向 `_fluxdns/test-temp`；结果为 635 通过、0 失败、2 个手动 profile 默认忽略。新增缓存/Storage/资源夹具也使用 `_fluxdns/tests/`。覆盖上表定向用例、namespace/插入年龄恢复、基础建表失败回滚、304 后取消不重试、parallel 负答案等待 Positive/超时不启动 fallback、非重试成员失败不遮蔽其他有效回答，以及独立子进程 panic 脱敏。既有地址缓存 TTL/换代、HTTP/HTTPS/SOCKS5、跨 UDP/TCP/DoH reload 与最终采样回归仍通过。

`cargo check --manifest-path backend/Cargo.toml --locked` 无警告通过；`cargo fmt --manifest-path backend/Cargo.toml -- --check`、`pwsh -File .agents/skills/project-doc-maintenance/scripts/check-docs.ps1` 与 `git diff --check` 均通过。文档检查覆盖 42 个 Markdown、517 个链接/引用，不验证外链网络或产品语义。

本机性能对比入口为 `cargo test --manifest-path backend/Cargo.toml --locked service::tests::benchmark_udp_telemetry_sampling_profile -- --ignored --nocapture`。它使用 debug 构建、4 个 runtime worker、单 UDP client，每秒一批 1,000 次 hosts 查询，至少覆盖两个 5 秒采样周期；保留真实 Resolution/SQLite 后台任务，JSON 输出写 sink，不测磁盘日志吞吐。延迟只统计实际请求，不含批次间等待，输出的 sequential_qps 不能视为整段负载吞吐。

该手动 profile 本次单独运行通过；旧的 `benchmark_udp_resolution_observation_profile` 未执行。本次单轮样本如下，不从此推断发布性能提升或稳定回归百分比：

| 模式 | 计时样本 | 平均 / p50 / p95 / p99（ms） | 最终资源状态 |
| --- | --- | --- | --- |
| 关闭 telemetry | 12,000 | 0.143020 / 0.126200 / 0.255300 / 0.322900 | source accepted 12,100（含预热），0 series，无 telemetry 任务 |
| 开启 telemetry | 12,000 | 0.153684 / 0.133200 / 0.255100 / 0.331400 | 同样 accepted 12,100，16 series，最终队列 0，输出 77 项，rejected_metrics 0 |

本次不修改 Storage 积压预算，不把短时 profile 外推为稳定压力通过；积压保护继续在[契约差距计划](../../plans/backend-contract-gaps.md)验收。真实远程、真实磁盘满/权限/介质故障、Unix 信号、发布硬件 SLO 和长期 RSS/CPU 压测均未执行；SQLite 写锁和 trigger 故障只证明已列出的本地分支。
