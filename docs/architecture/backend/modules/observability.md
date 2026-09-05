# Observability 模块设计

> 文档状态：有效
>
> 适用范围：tracing、metrics、health、脱敏、backpressure 和 telemetry 生命周期
>
> 最后评审：2026-09-05（完成事件后台 histogram/outcome/cache status 聚合；运行证据见[后台服务](../../../implementation/backend/background-services.md)）
>
> 关联实现：[observability.rs](../../../../backend/src/observability.rs)、[ports/telemetry.rs](../../../../backend/src/ports/telemetry.rs)、[resolution.rs](../../../../backend/src/resolution.rs)、[service.rs](../../../../backend/src/service.rs)
>
> 关联文档：[后端架构](../overview.md) · [配置字段参考](../../../implementation/configuration.md) · [Ports](ports.md) · [Runtime](runtime.md) · [Storage](storage.md)

## 1. 职责与边界

Observability 模块实现 Ports 定义的 telemetry/metrics 契约，提供：

- bootstrap 与正式 tracing subscriber；
- 结构化服务事件；
- 低基数 typed metrics 事件与运行计数；
- component health/degraded 状态；
- tracing event 到 typed log 的安全投影；
- redaction、有界队列、flush。

`ports/telemetry.rs` 定义可记录的数据契约；本模块选择 tracing 和具体 writer。Storage 的 resolve log 是业务详情存储，不等同于服务日志。

## 2. 初始化

两阶段：

1. bootstrap：stderr、固定 info/error 格式，用于 CLI/config/telemetry 初始化错误；
2. final：读取 `ResolvedConfig.logs` 后切换共享输出目标和 reloadable level filter，并把安全字段映射到 typed tracing layer，再写入 `TelemetryWriter`。

`logs.enable=false` 时关闭常规服务日志，但启动 fatal 和最终退出原因仍写 stderr。`logs.enable=true` 时写配置路径的追加文件，并保留 fatal stderr。

v1 日志级别固定接受 `trace`、`debug`、`info`、`warn`、`error`，大小写归一化；未知值由 Config 拒绝。

## 3. 日志格式

正式输出区分 `kind=log/metric/health` 的单行 JSON。log 字段是 `occurred_at_ms`、level、event、component、`has_request_digest`、`has_configured_id`、outcome、runtime_revision 和安全 message；digest/configured ID 在此输出为存在性，而非实际值。metric 输出受校验的 name/labels/value；聚合快照另带 writer_instance、occurred_at_ms 和 temporality，Counter 为 cumulative，Gauge 为 instantaneous。health 输出组件、状态、retry_count、stale_age_micros 和 persistence_gap。

`TypedTracingLayer::on_event` 只消费受支持字段，忽略任意 Debug payload；实际 LogEvent 的 request_digest/configured_id 在该桥接路径中均为 `None`。tracing 中出现了某个字段，不代表最终 JSON 会保留它。

字段集合由 typed event 构造，不允许业务代码随意添加任意 key。v1 不实现内建日志轮转；文件配额和轮转交给部署层，模块需正确处理写入失败并进入 degraded。

## 4. 请求关联边界

当前通过 typed request ID、resolution completion 与有限配置 ID 关联业务观测。源码没有接通原设计中的 process → runtime → request → policy/cache/group/attempt span 树；`TypedTracingLayer` 处理 event，不读取 span hierarchy。

独立 upstream attempt telemetry 同样未形成生产事件流；不得把 executor 的 attempt 列表、请求终态统计或早期测试类型当成完整 tracing。已决定本轮只扩展既有完成事件的后台指标，完整 span/attempt 诊断另案，不增加请求热路径同步调用。

## 5. Metrics

生产 `TelemetryWriter` 持有 [ObservabilityRegistry](../../../../backend/src/observability/registry.rs)，复用有界 series、原子 counter/gauge 与 checked add；不再保留独立 `EventWriter`、旧事件模型或第二份 health。`MetricsSink::record` 成功表示内存聚合已接受更新，不表示已经写入输出。Counter 接收增量，Gauge 覆盖当前值。

周期采样保留两个聚合描述符：

| 指标 | 类型与唯一标签 | 来源与口径 |
| --- | --- | --- |
| `ResolutionEventsAccepted` | Counter；`Component::Resolution` | `ResolutionPipelineMetrics.accepted`，是完成事件 ingress 接收数，不是所有 DNS 请求，也不等于持久化成功数 |
| `WriterQueueDepth` | Gauge；`Component::Telemetry` | flush 前 writer 的排队事件数，不含聚合 series |

Service 在既有 5 秒 flush 周期中采样，复用进程级 Source Arc 和共享游标；只在增量成功记录后推进游标，重复/最终采样不重复累计，源倒退明确报错。没有 Resolution owner 时只采样队列深度。`logs.enable=false` 不构造 writer/sampler，也不新增文件或任务。

后台 `ResolutionRuntime` dispatcher 在更新 stats 后，将同一完成事件交给 `TelemetryWriter::record_resolution`，增加固定 14 个 series：`RequestLatency`、`DnsCoreLatency` 两个 histogram，`RequestsTotal` 六种 outcome 与 `CacheOperations` 六种 cache status 计数。每项只使用固定 Component 和枚举标签，不为配置 ID、请求字段或逐 attempt 创建维度。请求包装层与 publisher 仍只有原有无等待移交；关闭详情不影响这些指标。

histogram 统一以微秒累计 count/sum，桶上界为 1/5/10/25/50/100/250/500/1000/2500/5000ms 与 `+Inf`。`RequestLatency` 来自既有毫秒字段，不因此提高采样精度；`DnsCoreLatency` 来自既有微秒字段。累计桶包含所有小于等于上界的样本。结构化快照增加 outcome/cache_status 与 histogram 的 unit/count/sum/buckets，temporality 为 cumulative，不导出原始样本。

两类聚合共享 128 series 硬上限，当前已接线的集合最多 16 项。`MetricsSink::record` 的通用入口仍仅开放上述两个周期采样描述符，完成事件使用专用 typed 入口。未知名称、错误类型/标签、溢出和容量耗尽明确拒绝，并增加固定 `rejected_metrics`；单个完成事件的多项更新全部成功才提交，不产生部分 histogram/count。所有快照 I/O 均在状态/registry 锁外执行。

输出重试可能再次写出相同累计值，消费者应按 writer 实例读取最新快照或求差，不能将周期快照相加。实例标识仅是输出元数据，不作为 label。输出失败保留聚合状态，下次只重试最新快照，不累积待发快照队列；Gauge 中间样本允许合并。

原有 `RequestLatency` / `UpstreamLatency` 的 `DurationMicros` port 输入仍走有界逐事件队列，与正式完成事件 histogram 是不同入口；没有独立 upstream attempt 采样或 HTTP exporter。聚合指标不占日志事件队列，队列满仍可更新。禁止 qname、完整 client ID、原始 IP、URL、header 或 error message 作为 label。

完成事件 histogram 统计 dispatcher 实际消费且指标接受的请求；`ResolutionEventsAccepted` 统计 ingress 入队，因此运行中可有排队差，关闭/溢出时也不能把二者强行当成相等或当成数据库成功数。

## 6. Health registry

每个组件发布状态：

- `healthy`；
- `degraded`；
- `failed`；
- `stopping`。

`TelemetryWriter` 的 health record 保存 first_seen、last_changed、last_success、retry_count、stale_age 和 persistence_gap；重复状态保留首次/最后变化时间及最大 stale age，Healthy 恢复时清除 stale age。它不保存通用 last_seen/原因字符串，也没有自动按时间窗口合并全部重复日志；该能力不能从早期 registry 实现外推。

Service、各 worker 和输出失败/恢复路径发布自身状态，Supervisor 决定其直接任务的重试/致命升级。Observability 保存并输出状态，不自行重启组件。

## 7. Redaction

永不记录：

- SecretRef 实际值；
- proxy URL credential；
- password hash 全文；
- TLS private key/certificate body；
- raw DNS wire；
- DoH query string/body；
- forwarded/PROXY 原始 header；
- 完整 client ID、原始 IP 或 ECS address；
- 远程规则正文。

允许的替代：

- keyed digest 或短期进程内 request ID；
- client bucket；
- IP family/prefix length；
- URL scheme/host digest；
- wire/latency bucket；
- 配置字段路径。

redaction 在 typed event 构造时完成，不依赖 formatter 最后补救。

## 8. Backpressure

`TelemetryWriter` 使用有界 non-blocking 内存队列，统一实现 `LogSink`、`MetricsSink` 和 `HealthSink`：

- debug/info 拥塞时可丢弃并计数；
- warn/error 优先淘汰已排队的低优先级日志；没有可淘汰项时返回明确的 `resource_exhausted`；
- 聚合 counter/gauge 不占事件队列，日志队列满时仍可更新；原始 duration/health 仍受事件队列容量约束；
- DNS 请求线程不等待磁盘；
- writer failure 保留失败事件并重新排队，返回安全分类错误供上层标记 degraded；
- telemetry/log writer 失败不影响 DNS response；
- telemetry 自身错误不递归写日志，具体 stderr/file fallback 由 output adapter 提供。

周期 flush 只处理开始时的有限事件批次，然后尝试聚合快照，避免持续入队饿死指标；正在输出的事件预留队列容量，失败重新入队不突破上限。日志输出失败可以提前终止该次 flush，但不会清除累计指标。同步输出前后检查同一 deadline，不承诺可强制中断已经阻塞的底层 Write。

shutdown 在 Resolution/Cache/Storage 回收后最后采样，再关闭 writer 输入、排空事件并输出最终聚合快照；已接受的 metric 更新必须包含在最终快照中，关闭后明确拒绝。周期与最终 flush 串行化并共用原有停机预算，不因重试延长 deadline。

## 9. 事件分类

event name 以实际调用点为准，例如 `configuration_validated`、`runtime_prepared`、`tcp_listener_failed`、`resolution_pipeline_shutdown_summary` 和 `service_shutdown`。不要将旧设计的 `runtime.prepare.*` / `upstream.attempt.complete` 等命名表视为已实现事件目录；完成事件也不意味着每个请求都写一条 service log。

事件名和字段变化视为内部观测 schema 变更，需更新 snapshot/golden tests。

## 10. 与 resolve log 的关系

- service log：组件运行与诊断，可丢弃低级事件；
- metrics：低基数聚合，进程内；
- stats：Storage 中默认持久化的产品统计；
- resolve log：Storage 中可选的单请求详情。

四者的持久化和容量边界分别实现；stats 与 resolve log 共享唯一的 typed resolution 完成事件来源，但使用独立下游队列。`logs.enable=false` 或详情队列满不能停止 stats；统一 resolution ingress 满则属于显式的整事件 gap。

resolve log 的服务端总耗时和 DNS 主链耗时在 core 完成时冻结，异步队列和持久化不改变其数值。它们是 authenticated 请求详情字段，不作为 metrics label，避免把高精度请求值引入无界指标维度。

## 11. Flush 与失败

`flush(deadline)` 在 deadline 内逐项输出，成功项计入 emitted，失败项重排队并保留 pending；超时显式返回，不能静默丢弃。`shutdown(deadline)` 先关闭新事件，再复用 flush。周期 task 与最终 flush 必须由进程 owner 管理，实际间隔和接线见[后台服务](../../../implementation/backend/background-services.md)。

shutdown 依赖顺序为：

1. 停止接收低优先级事件；
2. 写入最终 runtime/storage/cache 摘要；
3. drain 高优先级队列；
4. flush 文件；
5. 返回 dropped/failure 计数。

输出错误必须转换为安全 PortError。主目标失败可以尝试结构化 stderr fallback；双失败时重排队、更新进程内 Failed/retry，不能递归写故障日志。完整 flush 成功可恢复 Healthy，不用历史累计失败数误判当前健康。重复 degraded/failed 事件保留最大 stale age，恢复时清零；registry 跨 runtime revision 保留组件状态。

## 12. 契约验证要求

- bootstrap/final subscriber 切换；
- level 枚举和 filter；
- typed JSON event schema；
- secret、client、URL、header、wire redaction；
- high-cardinality label 拒绝；
- health 状态、重复状态归一化、恢复事件与输出失败；
- queue saturation、writer failure、stderr fallback；
- typed completion 关联；完整 request/group/attempt span 另列差距，不写成已有测试通过项；
- logs/metrics/stats/resolve log 相互独立；
- shutdown flush 和 dropped summary。
