# Runtime 模块设计

> 文档状态：有效
>
> 适用范围：Runtime 状态、listener 生命周期、后台任务监督、原子激活和 shutdown
>
> 最后评审：2026-09-05（模块边界与关键契约静态核对，基线见[模块索引](README.md)；不含运行验收）
>
> 关联实现：[snapshot.rs](../../../../backend/src/runtime/snapshot.rs)、[prepared.rs](../../../../backend/src/runtime/prepared.rs)、[coordinator.rs](../../../../backend/src/runtime/coordinator.rs)、[supervisor.rs](../../../../backend/src/runtime/supervisor.rs)、[service.rs](../../../../backend/src/service.rs)
>
> 关联文档：[后端架构](../overview.md)

## 1. 职责

Runtime 模块提供可服务状态、listener 生命周期、后台任务监督和原子激活原语。`DnsService` 是实际运行 owner，负责把这些原语与 Storage/Resolution/Telemetry/Management 组合；Application 调用它，不另建一套 task tree。

内部文件：

| 文件 | 职责 |
| --- | --- |
| `prepared.rs` | 构建无对外 socket 的候选运行时和 preflight |
| `snapshot.rs` | 不可变 `RuntimeSnapshot`、revision 和摘要 |
| `coordinator.rs` | `ActiveRuntime` 原子切换、CAS 合并、drain |
| `bind.rs` | `BindPlan`、socket 预创建、提交或回滚 |
| `supervisor.rs` | task tree、故障等级、重试与 shutdown |
| `system_socket.rs` | `socket2`/Tokio 系统 socket adapter 与不透明 I/O capability |
| `system_clock.rs` | `Clock` 的真实时间与 timer adapter |

## 2. 状态模型

类型与生命周期分层如下，括号部分是状态而非独立 Rust 类型：

```text
ResolvedConfig
  → PreparedRuntime
  → BoundCandidate
  → ActiveRuntime
  → ActiveRuntime（draining flag）
  → owner shutdown / handles released
```

- `RuntimeSnapshot`：请求热路径读取的不可变配置、策略、资源摘要、上游和 cache semantics；
- `PreparedRuntime`：已完成所有非 bind 准备，但不对外可见；
- `BoundCandidate`：全部目标 socket 已成功创建，尚未接收请求；
- `ActiveRuntime`：coordinator 的当前指针指向它，旧实例可因存量请求与 owner 引用继续存在；
- draining：同一 `ActiveRuntime` 的 admission flag 拒绝新 guard，不存在 `DrainingRuntime`/`Closed` 包装类型。

bind/activate 使用候选所有权和 CAS 区分准备与发布；listener 复用和资源合并仍会克隆共享句柄，不是全流程只进不出的线性类型状态机。

## 3. RuntimeSnapshot

`RuntimeSnapshot` 的直接字段是 revision、`Arc<ResolvedConfig>`、可选 `Arc<PolicyDnsCore>` 与 `Arc<ArcSwap<ResourceRegistrySnapshot<()>>>`。它没有独立的 upstream/cache/transport registry 字段。

compiled matcher、资源版本/hash 在 `PolicyDnsCore` 的 `ArcSwap<PolicyState>` 中；UpstreamRuntime、CacheFacade、LateCacheFinalizer 也由 core 持有。因此 snapshot 不直接暴露 socket/SQLx/Moka，但其 core 对象图可间接持有连接池、cache store 和后台 owner，不能宣称整个对象图只有纯数据。

service task 与请求入口必须使用同一 RuntimeRevision；transport 持有固定 runtime 句柄，显式 reload 才换代。resource worker 通过 coordinator 查询当前实例；Storage/Telemetry 按进程复用；历史和当前 finalizer owner 都受停机管理。Management listener 与用户 reload 边界遵循[管理面设计](../../management.md)，正式构造与调用顺序见[生命周期实现](../../../implementation/backend/lifecycle.md)。

正式 transport task 固定持有 `Arc<ActiveRuntime>` 及相同 revision 的 core；每个请求取得该 runtime 的 request guard，Policy core 再捕获一次 `Arc<PolicyState>`。它不是每请求重新读取 coordinator 当前指针；显式 reload 重建 transport task 后才换代。后台 optimistic refresh 可以通过 runtime core cell 选择最新 core，这是明确的独立执行窗口。

## 4. PreparedRuntime 构建

启动准备必须在 bind 前满足以下前置条件；这些职责由 Application、PreparedRuntime 和进程服务共同承担，不表示全部位于同一个构造器或严格按本表顺序执行：

1. 接收 `ResolvedConfig` 和各类 prepare plan；
2. 由 `StorageRuntime::open` 在共享 deadline 内打开必需业务数据库、执行 migration 及实际写入/回滚探针，见 [Storage](storage.md)；
3. 创建 storage、telemetry、cache 等 shared service；
4. 创建远程资源首次加载所需的 outbound/connector；
5. 对 remote rule-set 恢复已校验的落盘 pair，必要时下载、解析并原子持久化，再编译所有首次 resource snapshot；
6. 编译 client、strategy、route 和 upstream registry；
7. 创建 transport profile 和 bind plan；
8. 执行跨模块 preflight；
9. 生成候选 revision。

任一步失败都销毁候选资源，不改变当前 ActiveRuntime。首启时失败直接返回 Application。

## 5. BindPlan

Config 的 `BindPlan` 是经过校验的 `BindEntry` 列表，记录底层 UDP/TCP protocol、应用 transport、可选 `DohBindingRef`、address/port、owner 和 v6_only。TLS/client IP/route 配置不内嵌到 BindEntry；service 按 endpoint 引用构造 adapter，TaskSpec 另行定义故障策略。

绑定采用“全部成功再提交”：

1. 逐项通过 `SocketFactory` 创建未激活 socket；
2. 若任一失败，关闭本轮已创建 socket；
3. 全部成功后组合为 `BoundListenerSet`；
4. 激活前不启动 accept loop；
5. ActiveRuntime 原子发布后统一启动或放行 accept。

首启在全部 endpoint 成功后激活。配置切换允许复用未变 BindPlan 的 listener；需要 rebind 时，只有平台允许且所有新 endpoint 成功才切换。

## 6. Coordinator 与 CAS

`RuntimeCoordinator` 串行处理配置候选、资源更新和 fatal 状态迁移。Application 的配置 fingerprint 轮询只负责产生 reload 触发，不绕过 coordinator；资源刷新可以并行执行，但发布必须满足：

- 每个资源有独立 epoch；
- 结果 epoch 小于当前值时丢弃；
- 发布时基于最新 ActiveRuntime revision 合并 registry；
- CAS 失败后重新读取最新 registry 并重放本资源变更；
- 不从旧 registry 构造完整替换，避免并发更新互相覆盖。

刷新前后必须确认 captured ActiveRuntime 仍为当前实例；stale 结果跳过并重新捕获。复用 listener 前合并定义兼容且版本更新的资源状态，避免覆盖候选本身的新配置。资源-only 更新复用 listener/shared services；配置候选才执行 runtime swap。发布后旧 task 通过 scoped cancellation 退出，但仍有 late task 的 owner 必须保留到有界 drain 完成。

## 7. Supervisor

实际所有权分为 Supervisor 直接任务和独立服务内部任务：

```text
DnsService
  ├─ Supervisor
  │   ├─ UDP loop / TCP、DoH accept（内部 connection JoinSet）
  │   ├─ resource refresh / Storage periodic flush / Telemetry flush
  │   └─ Management server
  ├─ ResolutionRuntime（dispatcher / cache commit / detail projector）
  ├─ StorageRuntime（SQLite detail worker）
  └─ RuntimeCoordinator
      └─ current / historical LateCacheFinalizer（late JoinSet / persistence owner）
```

Supervisor 的 `TaskSpec` 只含 task ID、组件、restart policy 和 fault level；cancellation、JoinSet 及 restart count 由 Supervisor 运行状态持有。它没有通用的最近启动时间或逐 task shutdown-hook 字段。内部 worker 的 JoinHandle/JoinSet 由各自 owner 显式关闭，不等同于所有 panic 都会立即作为 Supervisor completion 上报。

一次性 task 与可重建 factory 分开注册；只有明确为 Transient 且未收到 shutdown cancellation 时才有界重试，每次创建新 future。reload 的 scoped token 仍属于全局 task tree。JoinSet task ID 必须映射完整 TaskSpec，panic/abort 按真实组件归因，不按注册顺序猜测；completion 和 shutdown report 保留重试次数。

无流量 receive/accept timeout 只推进轮询，不消耗 endpoint 重试。TCP/DoH session 有固定容量，满时暂停 accept。资源循环自己负责刷新 backoff，不叠加一套 task restart。单 endpoint 耗尽先按当前 revision 的逻辑 listener 聚合：仍有 sibling 则 degraded，全部不可用才致命升级；旧 revision 的迟到失败不能击穿新实例。瞬时重试复用已绑定句柄，不另设与 Runtime 竞争的自动 rebind。

## 8. 故障处理

故障等级：

- `request-local`：单请求解析、timeout、取消；
- `degraded`：资源刷新、详情 writer、cache persistence、telemetry/log writer 等可降级故障；
- `fatal-candidate`：候选 prepare/bind 失败，保留旧 runtime；
- `fatal-endpoint`：单 endpoint 达到重试上限；
- `fatal`：逻辑 listener 全部不可用、supervisor panic、启动必需 storage 失效或 shutdown timeout。

聚合统计数据库故障最初按 degraded 处理；pending batch 和补偿计数使用 v1 固定内存保护上限。达到上限时升级 fatal，停止接收新请求并在有限 flush 后退出，避免长期故障演变为 OOM。

Supervisor 瞬时重试采用确定性的指数退避：1ms 起、封顶 1,024ms，没有 jitter；重试次数受 `RestartPolicy::Transient` 限制。资源刷新使用自身 scheduler backoff，不复用这一数值。逻辑错误、schema 错误和 panic 不无限重启。

## 9. 请求计数与 drain

每个 ActiveRuntime 持有请求 guard 计数：

- accept 新请求时创建 guard；
- response 完成、客户端断开或取消时释放；
- 进入 drain 后拒绝新 guard；
- guard 归零即完成 drain；
- task cancellation 可在等待 grace deadline 之前中止 dispatch；总 deadline 用于限制回收等待，不保证所有存量响应完成。

UDP 无连接请求同样受 guard 约束。后台 cache finalizer 使用独立 semaphore、task 计数和 JoinSet，不增加 `ActiveRuntime` 的 request guard；coordinator 单独登记历史/当前 finalizer owner。request guard 的 Capacity 仅防计数溢出，不是全局并发配额。

## 10. Shutdown

顺序固定：

1. Management 撤销 session 并取消 accept；
2. `RuntimeCoordinator::begin_drain` 标记当前及历史 runtime，随后取消 transport/resource task；
3. Supervisor 取消并回收直接任务；TCP/DoH listener 取消并 join connection JoinSet；
4. coordinator 等待当前及旧 runtime 的 request guard 归零；
5. ResolutionRuntime 关闭 ingress 并依次排空 dispatcher/cache/detail；
6. coordinator 关闭历史/当前 LateCacheFinalizer 及其缓存持久化，合并安全计数；
7. StorageRuntime 关闭详情 worker、flush stats 并关闭业务数据库；
8. 最后关闭 Telemetry。

stats、resolve log、SQLite checkpoint 和 Telemetry flush 都必须纳入进程 drain。cache persistence 由 finalizer owner 有序关闭，安全摘要在 Telemetry 关闭前发布；未完成关闭与持久化失败/drop 分别记录，不能混成同一 timeout。实际 owner 和 task 接线见[后台服务](../../../implementation/backend/background-services.md)。

所有阶段共享调用方给定的总 deadline，阶段结果分别记录；不是每一步独立重置预算。“已读请求必须完成”不属于已接受的停机契约；后续并发、故障与 Unix 信号证据按[契约验证开发计划](../../../plans/backend-contract-validation.md)补齐，不借验收改为等待所有响应完成。

## 11. 契约验证要求

- prepare 任一步失败都不发布 candidate；
- 全部 bind 成功前没有 endpoint 接收请求；
- CAS 竞争不会丢失两个不同资源更新；
- 同一请求始终使用一个 runtime revision；
- 资源-only 更新复用 listener；
- rebind 失败保留旧 runtime；
- endpoint 重试、degraded、fatal 升级符合矩阵；
- drain 等待、deadline 取消和 flush 顺序可确定性验证；
- Supervisor 注册任务的 panic 可归因；内部 worker 的失败由相应 owner 回收，需分别验证，不以一类 task 的测试替代全部 owner。
