# Observability 模块设计

> 状态：v1 方案已完成，已实现有界低基数 metrics、health registry、retry/gap 计数和 typed event 脱敏；正式 writer/flush 尚未实现
>
> 更新日期：2026-08-31
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
2. final：读取 `ResolvedConfig.logs` 后构建正式 subscriber。

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

日志 writer 使用有界 non-blocking channel：

- debug/info 拥塞时可丢弃并计数；
- warn/error 尝试较高优先级队列或同步 stderr fallback；
- DNS 请求线程不等待磁盘；
- writer failure 标记 degraded；
- telemetry/log writer 失败不影响 DNS response；
- telemetry 自身错误避免递归写日志，使用最小 stderr fallback。

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

shutdown 在 deadline 内：

1. 停止接收低优先级事件；
2. 写入最终 runtime/storage/cache 摘要；
3. drain 高优先级队列；
4. flush 文件；
5. 返回 dropped/failure 计数。

文件不可写、磁盘满或 writer panic 时进入 degraded 并 fallback stderr；如果连 fatal stderr 都不可用，仍返回非零退出码，不能假装记录成功。

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
- [ ] 实现读取配置后的 final tracing；
- [x] 实现 redaction 和低基数校验；
- [ ] 实现有界 writer/backpressure；
- [x] 实现有界 health registry、状态恢复、retry/gap 计数和 typed event 更新；
- [ ] 实现 final writer/backpressure 和 flush；
- [x] 完成当前 schema、安全、低基数和状态测试；
- [ ] 完成 writer 故障注入和 flush 测试。

阶段 1 证据：bootstrap subscriber、日志级别解析、`Sensitive<T>`、DNS/resource/resolve event Debug 脱敏、metric label 类型匹配/去重/数量上限与敏感字段拒绝测试，以及 registry health/retry/gap focused tests 均通过；当前 backend 全量测试为 238 passed、0 failed。

当前实现进度：**30%**。
