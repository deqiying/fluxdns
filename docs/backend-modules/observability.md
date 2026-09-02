# Observability 模块设计

> 状态：v1 方案已完成，已实现有界低基数 metrics、health registry、retry/gap 计数、typed event 脱敏，以及面向稳定 telemetry ports 的有界 writer/backpressure、health lifecycle 与 stale age 归一化、deadline-aware flush、结构化文件/stderr 输出 adapter、主输出失败的 stderr fallback、Application 启动时输出目标和级别过滤切换；`TelemetryWriter` 已接入 `DnsService`/Supervisor 周期 flush 与 shutdown；typed final tracing layer 和 Storage/Telemetry/Supervisor/Resource refresh 的首轮 degraded health 发布已接入，跨 Runtime registry 生命周期和 fallback 失败后的最终处置仍待完善
>
> 更新日期：2026-09-02
>
> 目标代码：`backend/src/observability.rs`
>
> 上位设计：[后端架构](../backend-architecture.md) · [配置字段参考](../configuration-reference.md)
>
> 相关方案：[Ports](ports.md) · [Runtime](runtime.md) · [Storage](storage.md)

## 1. 职责与边界

Observability 模块实现 Ports 定义的 telemetry/metrics 契约，提供：

- bootstrap 与正式 tracing subscriber；
- 结构化服务事件；
- 低基数内存 metrics；
- component health/degraded 状态；
- request/attempt span 关联；
- redaction、限流、flush。

`ports/telemetry.rs` 定义可记录的数据契约；本模块选择 tracing 和具体 writer。Storage 的 resolve log 是业务详情存储，不等同于服务日志。

## 2. 初始化

两阶段：

1. bootstrap：stderr、固定 info/error 格式，用于 CLI/config/telemetry 初始化错误；
2. final：读取 `ResolvedConfig.logs` 后切换共享输出目标和 reloadable level filter，并把安全字段映射到 typed tracing layer，再写入 `TelemetryWriter`。

`logs.enable=false` 时关闭常规服务日志，但启动 fatal 和最终退出原因仍写 stderr。`logs.enable=true` 时写配置路径的追加文件，并保留 fatal stderr。

v1 日志级别固定接受 `trace`、`debug`、`info`、`warn`、`error`，大小写归一化；未知值由 Config 拒绝。

## 3. 日志格式

正式日志使用一行一个 JSON event，至少包含：

- UTC timestamp；
- level；
- event name；
- component；
- request/trace digest；
- listener/route/upstream/resource 等 typed ID；
- outcome/failure class；
- latency/size bucket；
- runtime revision；
- message。

字段集合由 typed event 构造，不允许业务代码随意添加任意 key。v1 不实现内建日志轮转；文件配额和轮转交给部署层，模块需正确处理写入失败并进入 degraded。

## 4. Span 模型

```text
process
  └─ runtime revision
      ├─ listener / endpoint
      │   └─ dns request
      │       ├─ policy decision
      │       ├─ cache lookup/refresh
      │       └─ upstream group
      │           └─ attempt
      └─ background task
          ├─ resource refresh
          ├─ stats batch
          └─ cache persistence batch
```

span ID 用于内部关联。请求量大时，不要求每个低级步骤都输出日志；采样不能影响聚合统计。

## 5. Metrics

v1 默认是进程内低基数 atomics/histogram facade，不暴露 HTTP exporter。至少记录：

- requests total、active、cancelled、failed；
- latency buckets；
- cache hit/miss/stale/write/reject；
- upstream attempt/outcome/latency；
- listener connection/admission/error；
- resource refresh success/failure/stale age；
- stats/detail/cache writer queue、drop、retry、gap；
- runtime revision、active/draining request；
- component health state。

标签只能来自有限枚举或配置定义 ID。禁止 qname、完整 client ID、原始 IP、URL、header 或 error message 作为 label。

## 6. Health registry

每个组件发布状态：

- `healthy`；
- `degraded`；
- `failed`；
- `stopping`。

状态记录 first_seen、last_changed、last_success、retry_count、stale_age、gap flag 和安全原因分类。重复相同错误只更新 counter/last_seen，按时间窗口限流日志，防止故障风暴。

Runtime supervisor 是状态生命周期的权威；Observability 只存储和输出状态，不自行重启组件。

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
- metrics/health 不等待输出端，队列已满时返回明确的容量错误；
- DNS 请求线程不等待磁盘；
- writer failure 保留失败事件并重新排队，返回安全分类错误供上层标记 degraded；
- telemetry/log writer 失败不影响 DNS response；
- telemetry 自身错误不递归写日志，具体 stderr/file fallback 由 output adapter 提供。

## 9. 事件分类

建议固定 event name：

- `runtime.prepare.*`、`runtime.activate`、`runtime.shutdown.*`；
- `listener.bind.*`、`listener.accept_error`；
- `dns.request.complete`；
- `cache.lookup`、`cache.refresh.*`、`cache.persistence.*`；
- `upstream.attempt.complete`、`upstream.group.complete`；
- `resource.refresh.*`；
- `storage.stats_batch.*`、`storage.resolve_drop`；
- `component.state_change`。

事件名和字段变化视为内部观测 schema 变更，需更新 snapshot/golden tests。

## 10. 与 resolve log 的关系

- service log：组件运行与诊断，可丢弃低级事件；
- metrics：低基数聚合，进程内；
- stats：Storage 中默认持久化的产品统计；
- resolve log：Storage 中可选的单请求详情。

四者分别实现，不能因为 `logs.enable=false` 或详情队列满而停止 stats。

## 11. Flush 与失败

当前 writer 的 `flush(deadline)` 在 deadline 内逐项调用 `TelemetryOutput`，成功项计入 emitted，输出失败项放回队首并保留 pending；deadline 到期返回 timeout，不会静默丢失队列。`shutdown(deadline)` 先关闭新事件，再复用同一 flush 边界。生产 `DnsService` 通过 Supervisor 每 5 秒执行一次 bounded flush；服务 shutdown 先停止该 task，再在统一 deadline 内执行最终 `TelemetryWriter::shutdown`。

TelemetryWriter 接入后的 shutdown 顺序为：

1. 停止接收低优先级事件；
2. 写入最终 runtime/storage/cache 摘要；
3. drain 高优先级队列；
4. flush 文件；
5. 返回 dropped/failure 计数。

`StructuredTelemetryOutput` 已提供真实文件与 stderr 目标，并将文件写入/flush 错误转换为安全 `PortError`；主输出失败时会尝试写入安全结构化 stderr，只有 fallback 也失败才返回错误并由 `TelemetryWriter` 重排队。`run` 在严格配置和 SecretRef 校验后切换共享 bootstrap 输出目标及 reloadable level filter，并将 tracing layer reload 为 typed layer；`logs.enable=false` 时丢弃普通日志。Storage flush、Telemetry flush、Supervisor fatal、Resource refresh 和 shutdown 已发布首轮 `ComponentHealthEvent`；writer 会在 degraded/failed 重复事件间保留最大 `stale_age_micros`，并在恢复 `Healthy` 时清零。fallback 失败后的 health registry 最终处置、跨 Runtime 生命周期和真实 writer panic 复现仍待完善。

## 12. 测试

- bootstrap/final subscriber 切换；
- level 枚举和 filter；
- typed JSON event schema；
- secret、client、URL、header、wire redaction；
- high-cardinality label 拒绝；
- health 状态、重复错误限流和恢复事件；
- queue saturation、writer failure、stderr fallback；
- request/group/attempt span 关联；
- logs/metrics/stats/resolve log 相互独立；
- shutdown flush 和 dropped summary。

## 13. 实现检查清单

- [x] 定义 typed event/metric/health types；
- [x] 实现 bootstrap tracing；
- [x] 实现读取配置后的 final tracing；
- [x] 实现 redaction 和低基数校验；
- [x] 实现稳定 telemetry ports 的有界 writer/backpressure；
- [x] 实现有界 health registry、状态恢复、retry/gap 计数和 typed event 更新；
- [x] 实现 writer flush/requeue/deadline 边界；
- [x] 完成当前 schema、安全、低基数和状态测试；
- [x] 完成 writer 输出故障、队列拥塞、deadline 和 shutdown 定向测试；
- [x] 实现结构化真实文件/stderr output adapter；
- [x] 在 Application 启动阶段按 `logs.enable/path` 切换共享输出目标；
- [x] 在 Application 启动阶段按 `logs.level` 切换 reloadable level filter；
- [x] 将 `TelemetryWriter` 接入 `DnsService`/Supervisor 周期 flush 和 shutdown；
- [x] 接入 typed final tracing layer，并把安全 `event/component/result/revision` 字段写入 `TelemetryWriter`；
- [x] 接入 Storage/Telemetry/Supervisor/Resource refresh 的首轮 degraded/failed/stopping health 发布；
- [x] 在 Telemetry 关闭前记录 Storage 正常停机的提交、积压、失败和丢弃纯计数摘要；
- [x] 在 `TelemetryWriter` 内归一化组件 health lifecycle 字段，保留首次时间、最近成功、累计重试和 gap 语义；
- [x] 在 degraded/failed health 生命周期中传播 stale age，并在恢复 `Healthy` 时清零；
- [ ] 完善 health registry 的跨 Runtime 生命周期和 fallback 失败后的最终处置。

阶段 1/9 证据：bootstrap subscriber、日志级别解析、`Sensitive<T>`、DNS/resource/resolve event Debug 脱敏、metric label 类型匹配/去重/数量上限与敏感字段拒绝、registry health/retry/gap，以及 `TelemetryWriter` 的容量、优先级、flush/requeue/deadline/shutdown focused tests 均通过；阶段 79 新增 `StructuredTelemetryOutput` 文件输出 adapter，阶段 80 接入 Application 启动时输出目标切换，阶段 81 接入 reloadable level filter，阶段 90 将 writer 接入 Supervisor 周期 flush/shutdown，阶段 91 接入 typed tracing layer，阶段 92 接入首轮 health publish，阶段 93 接入主输出失败 stderr fallback，阶段 94 接入 Resource refresh health publish，阶段 96 接入 health lifecycle 归一化；Observability focused tests `20 passed、0 failed`，typed layer、fallback 和 lifecycle focused test 各 `1 passed、0 failed`，service telemetry flush task 和 health publish 各 `1 passed、0 failed`，service focused suite `29 passed、0 failed`，并通过 `cargo check`/`cargo clippy --all-targets -D warnings`。跨 Runtime health registry 生命周期和 fallback 失败后的最终处置仍待后续切片。

阶段 133 在既有 StorageRuntime 受监督 shutdown 路径增加 `Stopping` health 和 `storage_shutdown_summary`，所有字段均为计数或布尔状态，并通过原有停机定向测试。

阶段 151 补齐 `TelemetryWriter` 的 stale age 生命周期归一化，并由 Observability 20 项定向测试验证重复故障保留和健康恢复清零。

当前实现进度：**92%**。
