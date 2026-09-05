# Observability 模块设计

> 文档状态：有效
>
> 适用范围：tracing、metrics、health、脱敏、backpressure 和 telemetry 生命周期
>
> 最后评审：待核对（本次仅分类与边界复核，不等同完整契约重审）
>
> 关联实现：`backend/src/observability.rs`
>
> 关联文档：[后端架构](../overview.md) · [配置字段参考](../../../implementation/configuration.md) · [Ports](ports.md) · [Runtime](runtime.md) · [Storage](storage.md)

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
- `resolution_pipeline_shutdown_summary`；
- `component.state_change`。

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
- health 状态、重复错误限流和恢复事件；
- queue saturation、writer failure、stderr fallback；
- request/group/attempt span 关联；
- logs/metrics/stats/resolve log 相互独立；
- shutdown flush 和 dropped summary。
