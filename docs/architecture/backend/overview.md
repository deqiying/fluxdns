# 后端总体设计

> 文档状态：有效
>
> 适用范围：后端跨模块依赖、运行时所有权、请求与副作用边界
>
> 最后评审：2026-09-05（跨模块边界复核；各模块沿用独立评审状态）

## 职责与依赖

Application 是装配入口，Runtime 管理候选、活动实例、bind、任务监督与关闭；领域逻辑通过 ports 使用 transport、upstream、cache、storage 和 telemetry adapter。

| 层 | 负责 | 不允许 |
| --- | --- | --- |
| DNS / Policy / 领域模型 | canonical message、策略决策、缓存语义与结果分类 | 在公开 port 泄漏 HTTP、SQLx、Moka、TLS 或 YAML DTO |
| Ports | 请求、exchange、缓存、完成事件、存储、观测和副作用契约 | 把具体 client/connection 当成领域 API |
| Adapters | 协议 framing、网络、Moka、SQLite、文件和远程资源 | 在内部另行解释配置继承或策略优先级 |
| Runtime / Service | prepare、bind/CAS、listener/resource task 和进程服务生命周期 | 请求中加载 YAML；失败候选替换活动实例 |
| Application | CLI、依赖装配、信号和错误码 | 同时持有一套独立 supervisor 或业务实现 |

这是逻辑职责图，不是源码层级声明。组合根可以构造具体 adapter；公共核心契约保持协议无关。`dns/policy.rs` 内含 adapter 装配的实际边界见[DNS 实现](../../implementation/backend/dns-pipeline.md)。完整职责分解见[模块索引](modules/README.md)。

依赖选择保持有意约束：Tokio 管理异步 I/O；Hickory 只负责低层 DNS wire；DoH 入站使用有界 HTTP/1.x adapter，Management 使用独立 Axum adapter；Reqwest connector 必须禁用默认 system proxy，复用按上游/outbound 建立的连接能力。Moka 与 SQLite 经 port 接入。版本和 feature 以 [Cargo.toml](../../../backend/Cargo.toml)、[Cargo.lock](../../../backend/Cargo.lock) 为准，不在设计中维护检索版本表。

## 配置与运行时

```text
RawConfigVn -> migration / strict parse -> ResolvedConfig
            -> PreparedRuntime(candidate, no bound listener)
            -> bind / reuse endpoints -> ActiveRuntime
```

- `ResolvedConfig` 表达已归一化的配置语义，不等于可服务 runtime。
- prepare 完成引用图、策略、connector、资源初始 snapshot 和缓存恢复准备；首次必需资源失败不进入 bind。
- `RuntimeSnapshot` 保存请求所需稳定配置/索引与能力，不持有 socket、HTTP connection 或数据库连接。
- `ActiveRuntime` 持有已绑定入口和共享能力；配置候选完整准备后通过 revision CAS 切换，旧实例进入 drain。
- 资源刷新使用 per-resource epoch/hash/parser version 与 CAS。旧 runtime 或旧 epoch 的刷新结果不得覆盖新实例或其他资源；资源-only publish 不重绑 listener。
- Storage、Telemetry 和解析事件管线按进程管理，reload 重用其 sink；历史/当前 finalizer owner 都必须参与有界 shutdown。

配置字段和 restart-required 范围见[配置参考](../../implementation/configuration.md)，实际顺序见[生命周期](../../implementation/backend/lifecycle.md)。

## 请求与缓存

```text
InboundAdapter -> canonical query + RequestContext
 -> PolicyContext -> fast cache lookup
 -> on miss: RouteDecision -> hosts / resolved cache / upstream
 -> canonical response + completion
 -> one nonblocking ResolutionEnvelope -> ResponseEncoder
```

DoH route 在 adapter 用共享模板匹配一次，Policy 按 typed route ID 查表。请求一次捕获 runtime；client/strategy/namespace/ECS 等上下文在 fast lookup 前确定，逐规则 matcher 只在 miss 后运行。

缓存 key 的 `Fast`/`Resolved` mode 隔离；fingerprint 包含会改变答案的策略/资源语义，不包含整个 runtime revision 或纯观测变化。资源更新切换 key 而不扫描全库，旧项按 TTL/容量淘汰。不能预先确定的 group member ECS 必须绕过不安全的缓存复用。listener/strategy hosts 本地回答绕过 response cache，上游 hosts connector 则遵循上游响应准入。

缓存保存 canonical response 和 producer provenance，不保存客户端 ID 或 transport envelope。首响应与 late-window 缓存候选分离：late result 不能改写已返回响应。single-flight lease 必须在成功、队列丢弃、取消和 drop 上都有终态，不能永久挂起 waiter。细节分别见 [DNS Core](modules/dns-core.md)、[Cache](modules/cache.md) 和 [Upstream](modules/upstream.md)。

## 副作用、失败与监督

- 核心返回后、transport 编码前，最多无等待发布一次完成事件。ingress、cache commit 与详情队列的 gap 分别计数，不把丢失伪装为已落库。
- Stats 使用有界维度、UTC day、epoch snapshot 和 batch ledger；同一批重试幂等，一次请求只增加一次请求数，parallel attempt 不扩成多条请求。
- 启动时统计数据库打开/migration 失败为 fatal；缓存恢复失败可降级为纯内存。运行期普通数据库错误保留 pending 重试，超过内存保护或不可恢复错误升级处理。
- 服务任务由 Supervisor 注册，区分可降级组件与致命入口；重试、取消和 drain 受 deadline 限制，不能无界等待。
- 一般日志和 metrics 不记录 qname、原始 IP、Secret 或高基数内容；受保护的查询详情属于独立受限数据集，见 [Management](../management.md) 与 [Storage](modules/storage.md)。

## 验证约束与现状入口

模块中的“契约验证要求”是应证明的行为，不是测试通过记录。跨 adapter conformance、真实磁盘故障、Unix 进程信号及目标负载压力的未闭合项保留在[契约核对计划](../../plans/backend-contract-gaps.md)。不以本机 loopback、fake 或静态接线替代目标环境验收。

[生命周期](../../implementation/backend/lifecycle.md) · [DNS 管线](../../implementation/backend/dns-pipeline.md) · [后台服务](../../implementation/backend/background-services.md) · [管理端](../../implementation/backend/management.md)
