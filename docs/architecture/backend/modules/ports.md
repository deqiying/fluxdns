# Ports 模块设计

> 文档状态：有效
>
> 适用范围：DNS Core 与 adapter 之间的稳定接口、错误语义和 contract test
>
> 最后评审：2026-09-05（模块边界与关键契约静态核对，基线见[模块索引](README.md)；不含运行验收）
>
> 关联实现：[ports/mod.rs](../../../../backend/src/ports/mod.rs)、[inbound.rs](../../../../backend/src/ports/inbound.rs)、[effects.rs](../../../../backend/src/ports/effects.rs)、[testing.rs](../../../../backend/src/ports/testing.rs)
>
> 关联文档：[后端架构](../overview.md)

## 1. 目标

Ports 模块定义 DNS 核心与外部副作用之间的稳定契约。核心业务只依赖 port 数据类型和行为语义，不依赖 Axum、Reqwest、SQLx、Moka、Rustls、Tokio socket 或 YAML DTO。

目录职责：

| 文件 | 契约 |
| --- | --- |
| `inbound.rs` | 入站请求、响应关联、transport capability |
| `exchange.rs` | 上游 exchange、connector、选择结果 |
| `cache.rs` | 内存缓存和持久化缓存能力 |
| `observation.rs` | 单次解析完成事件、详情 typed source 与非阻塞发布 |
| `storage.rs` | migration、统计、解析详情、flush/shutdown |
| `management.rs` | Management overview、分页统计和解析详情的只读查询及安全投影 |
| `telemetry.rs` | structured event、metrics、组件健康状态 |
| `effects.rs` | clock、resource fetcher、secret、socket factory |

Ports 不包含具体 adapter，也不成为“所有类型都抽象成 trait”的通用层。

## 2. 设计原则

- 热路径数据结构保持小而不可变，优先静态分发；
- 只有需要运行时注册或测试替换的边界使用 `Arc<dyn ...>`；
- port 不暴露第三方 crate 的 request、response、connection 或 error 类型；
- 异步 I/O 按具体签名接收或继承 deadline/cancellation；storage/cache 部分操作只有 deadline，同步 `try_publish`/`emit` 不另加异步预算；
- 所有 port 错误先按行为分类，仅保留静态 operation 和可选安全上下文，不携带 adapter 原始错误文本；
- request/response 关联只能完成一次。

## 3. 入站契约

`InboundAdapter` 产出：

- `CanonicalQuery`；
- `RequestContext`；
- 只读的 response correlation handle。

`ResponseEncoder` 接收共享的 `Arc<CanonicalResponse>`，负责把 canonical response 变回 UDP/TCP/HTTP envelope，包括 DNS ID 恢复、UDP 截断、TCP length prefix 和 HTTP headers。Core 不直接调用 socket 或构建 HTTP response；详情投影与 transport encoder 复用同一响应对象。

`effects.rs` 中的 `TcpReadResult` 将完整 frame 与 clean EOF 分开：没有开始新 frame 的 EOF 是连接正常结束，已读入前缀或 payload 后的 EOF 归为协议错误。TCP session 由 transport 持有连接级 correlation，Ports 不暴露 Tokio stream 类型。

`TcpReadChunkResult`/`read_chunk` 为 HTTP 等可变长度协议提供 bounded byte-stream capability；`max_bytes`、deadline、cancellation 和 clean EOF 仍由 port contract 约束，系统实现不泄漏 Tokio 类型。

`TlsServerMaterial` 只携带已加载的 DER 证书链和私钥字节，并提供脱敏 `Debug`；`TcpListenerHandle::accept_with_tls` 与 `TcpConnectionHandle::start_tls` 为不支持 TLS 的 fake/listener 保留明确的兼容错误。

关联 handle 的状态机：

```text
Pending → Responded
Pending → ClientGone
Pending → Cancelled
```

任何重复响应返回可分类错误，不得第二次写 socket/stream。

## 4. 上游契约

`DnsExchange` 输入 canonical query 与 request context，输出统一结果：

- `Response(CanonicalResponse)`：terminal DNS response，协议合法且问题段匹配；
- `TransportFailure`：连接、TLS、HTTP、超时、wire 或问题段错误；
- `Cancelled`：request deadline、client disconnect、shutdown 或 group policy 取消。

DNS RCODE 不是 transport failure。`NXDOMAIN`、`REFUSED`、`SERVFAIL` 和 `TC=1` 都可作为 terminal response，是否缓存由 Cache/DNS Core 决定。

`UpstreamConnector` 是已经绑定 profile 的 handle；URL、代理、TLS、连接池和 bootstrap 状态不能重新暴露给 DNS Core。

## 5. 缓存契约

`CacheStore` 只提供实现 CacheFacade 所需的最小能力：

- 按完整 key 读取；
- 条件插入/替换；
- 按 key、namespace 或 predicate 显式失效；
- per-entry expiry；
- single-flight 所需的原子占位或等价机制；
- 关闭与统计。

`PersistentCacheStore` 另行负责批量加载、批量持久化、format/version/checksum 校验和容量维护。它与业务 `StorageBackend` 是两个不同 port。

single-flight producer 的写入责任由不可 clone 的 `CacheCommitCandidate` 表达。candidate 持有请求、共享响应和 RAII lease；后台 commit 成功、拒绝、冲突、不可用或 candidate drop 都必须结束 lease，使 follower 取得确定终态。

## 6. 解析完成事件契约

`ResolutionEventSink::try_publish` 是 producer 唯一可见的 resolution 事件入口：

- 每个请求在 transport encode 前至多调用一次，使用有界、无等待语义；
- `ResolutionEvent` 保存低基数终态、cache lookup status、在 core 完成时冻结的总耗时/主链耗时和可选 `ResolutionDetailSource`；
- 详情 source 只保存 typed question、有效 client IP 与共享 response，qname digest/answer JSON 必须在后台生成；
- `ResolutionEnvelope` 一次性携带事件与可选 `CacheCommitCandidate`，避免两个独立发布路径产生部分成功；
- ingress 满时返回 `DroppedQueueFull`，DNS 响应继续，系统累计 dropped 与首次 gap 时间；关闭后返回 `Disabled`/稳定错误；
- `detail_enabled()` 是进程启动时冻结的 interest gate，关闭详情时 producer 不构造请求级详情 payload。

耗时跨 port 时只传递非负整数数值，不传递 `Instant`。总耗时从 transport 计时点到 DNS core 完成，主链耗时只覆盖 `DnsCore::resolve_with_completion`；后台 dispatcher/detail/SQLite 的排队与执行时间不得进入两者。

stats、cache commit 和 detail projection 是 dispatcher 的三个独立消费者。cache lookup status 只描述响应完成前的 hit/miss/stale；异步 commit outcome 单独计数。

## 7. 存储契约

`StorageBackend` 提供业务数据库的：

- schema migration；
- 事务执行；
- health probe；
- flush、checkpoint 和 shutdown。

`StatsRecorder` 是 resolution dispatcher 的内部消费端，不再由 DNS 请求任务直接调用。`ResolveEvent` 只保留为存储 transaction 使用的持久化 DTO，不是另一个可发布 port。详情 projector 与 SQLite writer 各自使用有界 channel；详情队列满只能影响详情，不能反向阻塞 stats 或 cache commit。统一 ingress 自身溢出则属于整条 resolution event gap，必须显式计数。

`ManagementStorageRead` 只接受 UTC day、分页和有限 enum filter/sort。authenticated queries 投影允许返回 canonical qname、配置客户端名称、有效 client IP、strategy、target/actual upstream 与有界 answer；仍不返回 DNS wire、request digest、route 文本或数据库 row ID。HTTP handler 只依赖该 port；SQLite adapter 自行负责 opaque ID、固定 SQL 模板、绑定参数、历史 `legacy_redacted` 映射和 read-only 连接。

## 8. Telemetry 与副作用

`LogSink`/`MetricsSink` 只接受已经脱敏、标签集合受限的事件；`HealthSink` 接收 `ComponentHealthEvent`。三个 trait 均由 `TelemetryWriter` 实现，Ports 只定义字段与语义。Health 有独立事件与状态记录，不能把每次 health 更新等同于已写入某个 metrics gauge。

`MetricsSink::record` 对已注册 Counter/Gauge 表示内存聚合接受，不是逐次输出：Counter 输入增量，Gauge 输入当前值；duration 仍是受限的原始样本队列。描述符、错误、采样来源与 cumulative/instantaneous 快照契约见 [Observability](observability.md)，不从 `MetricName` 枚举存在推断生产调用点。

`effects.rs` 提供：

- `Clock`：monotonic time、UTC time、sleep/timer；
- `ResourceFetcher`：受 deadline、proxy profile 和最大体积约束的资源读取；请求携带有界不透明验证器，结果区分 `Modified(ResourceContent)` 与 `NotModified(ResourceValidators)`；可选 `validator_scope` 隔离 adapter 配置代际，默认无 scope 不复用条件标记，详见 [Resource](resource.md)；
- `SocketFactory`：创建未激活 socket，供 BindPlan 统一提交。

`SocketSpec` 同时携带 `kind`、目标地址、`reuse_port` 和 IPv6 `v6_only` 选择；Runtime 在 bind 阶段只通过该契约传递平台相关选项，不向 DNS Core 泄漏 socket 类型。

文件系统和网络 I/O 不通过“万能 effects trait”合并，避免接口失去约束。

secret 读取由 Config 的 `ResolvedSecretRef` accessor 负责，复用其限额、校验和 `ResolvedSecretValue` 脱敏包装，不另设未被调用的 secret port。

当前 adapter 所有权为：

| Port | Adapter 所有者 |
| --- | --- |
| `Clock` | Runtime 的 system clock/timer adapter |
| `ResourceFetcher` | Resource 的 `ReqwestResourceFetcher`，复用 Upstream outbound profile |
| `SocketFactory` | Runtime `SystemSocketFactory`，由 `bind.rs` 编排，基于 `socket2` 创建 socket |

## 9. Deadline 与取消

所有 port 遵循：

1. deadline 只能缩短，不能被下游延长；
2. cancellation 原因必须保留；
3. 取消是正常控制流，不记录为组件 error；
4. 一个 waiter 取消不能终止仍被其他 waiter 使用的 single-flight；
5. 从请求任务移交成功后的 cache commit 使用独立的 100ms deadline，不继承已经完成的客户端 deadline；
6. shutdown 取消优先于后台刷新，但不得跳过有限 flush，未提交 candidate 必须通过 drop 释放 lease。

## 10. 错误分类

port error 至少包含：

- `InvalidInput`；
- `Timeout`；
- `Cancelled(reason)`；
- `Unavailable`；
- `PermissionDenied`；
- `ResourceExhausted`；
- `ProtocolViolation`；
- `CorruptData`；
- `Internal`。

`PortError` 只存 `PortErrorClass`、`&'static str` operation 和可选 `&'static str` safe_context；Core 匹配稳定分类。外围 typed error 可以包裹安全 `PortError`，但不能借 source chain 重新暴露原始 URL、wire 或 credential。

## 11. Contract test kit

`testing.rs` 提供 FakeClock、FakeExchange、FakeInboundAdapter、FakeResponseEncoder、FakeCacheStore、FakeTelemetry 等共享夹具；缓存和存储另有 `backend_contract_tests.rs`。以下是跨 adapter 应覆盖的行为：

- 给定 canonical query，所有 inbound adapter 产出等价 context；
- response encoder 只能完成一次；
- fake clock 可确定性推进 timeout、TTL 和 retry；
- fake exchange 能产生 terminal/failure/cancelled 矩阵；
- fake store 能模拟 CAS 竞态、队列满和恢复；
- fake telemetry 能拒绝高基数或敏感字段。
- capturing observation sink 能验证 exactly-once、typed payload、共享 response identity 和 queue-full disposition。

生产 Runtime 不执行 contract suite，也没有统一的 conformance 注册门禁。测试夹具存在不等于每个 adapter 的所有组合都已验收；已覆盖用例与剩余矩阵须按[契约验证开发计划](../../../plans/backend-contract-validation.md)的 V3/V6/V7 分别核验。
