# Runtime 模块设计

> 状态：v1 方案已完成，阶段 3 基础服务编排、Runtime 资源摘要和 service core 构造入口已实现；`PreparedRuntime`/`ActiveRuntime` 现已持有生产 `ResourceFetcher`，async `PreparedRuntime` 已在 bind 前完成 remote restore-or-fetch、file hosts/rule-set compiled snapshot 构造，`auto_update=true` 的 remote/file refresh task 已纳入 service Supervisor，并在当前 ActiveRuntime 原子更新 Policy 与资源摘要；`Application` 与 `DnsService` 现已共享持有 `RuntimeCoordinator`，资源循环通过 coordinator 读取当前活动实例，coordinator 还提供候选 `bind_and_activate` 入口和 stale-active refresh guard，Application 提供配置文件 reload 触发 API，service 会观察 Supervisor 终止 task 并按 fault level 升级不可恢复故障；Supervisor 已支持受管 task-scoped cancellation，DnsService 的 UDP/TCP/DoH listener 与 resource refresh task 已使用独立 scoped token，显式 service reload 已可按 BindPlan 变化选择重绑或复用 listener，并重建 listener/resource task；database/logs/webui/resolve-log 等进程持有配置变化会拒绝热重载并保留旧 Runtime；候选 revision CAS 现已在 mutation gate 下合并兼容资源的更高版本、Policy compiled index、Runtime metadata 和 worker 稳定调度状态，`run` 已通过 Application 的配置 fingerprint 轮询复用 service reload，当前 Runtime 已提供 deadline-aware request drain wait，旧 draining Runtime owner 会在新激活时惰性清理，并在 owner 淘汰时同步清理无活动 `LateCacheFinalizer` owner，运行中 service 已验证资源 refresh 对同一 listener 的 live publish；资源内容刷新明确不创建新 Runtime，完整跨 Runtime shared-service 与 flush 生命周期仍在后续阶段
>
> 更新日期：2026-09-03
>
> 目标代码：`backend/src/runtime/*`
>
> 上位设计：[后端架构](../backend-architecture.md) · [开发计划](../backend-development-plan.md)

## 1. 职责

Runtime 模块拥有可服务状态、listener 生命周期、后台任务监督和原子激活。Application 只发起 prepare/serve/shutdown，不能直接持有服务 task。

内部文件：

| 文件 | 职责 |
| --- | --- |
| `prepared.rs` | 构建无对外 socket 的候选运行时和 preflight |
| `snapshot.rs` | 不可变 `RuntimeSnapshot`、revision 和摘要 |
| `coordinator.rs` | `ActiveRuntime` 原子切换、CAS 合并、drain |
| `bind.rs` | `BindPlan`、socket 预创建、提交或回滚 |
| `supervisor.rs` | task tree、故障等级、重试与 shutdown |
| `system_socket.rs` | `socket2`/Tokio 系统 socket adapter 与不透明 I/O capability |

## 2. 状态模型

状态严格分层：

```text
ResolvedConfig
  → PreparedRuntime
  → BoundCandidate
  → ActiveRuntime
  → DrainingRuntime
  → Closed
```

- `RuntimeSnapshot`：请求热路径读取的不可变配置、策略、资源摘要、上游和 cache semantics；
- `PreparedRuntime`：已完成所有非 bind 准备，但不对外可见；
- `BoundCandidate`：全部目标 socket 已成功创建，尚未接收请求；
- `ActiveRuntime`：唯一对外服务实例；
- `DrainingRuntime`：不再接收新请求，只等待存量请求和 flush。

类型转换消耗前一状态的所有权，避免把“半准备”对象误发布。

## 3. RuntimeSnapshot

snapshot 只持有请求所需的不可变 handle：

- revision 与 normalized config hash；
- policy index；
- per-resource registry snapshot（由 `ArcSwap` 持有资源元数据摘要，compiled payload 仍由 `PolicyDnsCore` 持有）；
- upstream connector registry；
- cache semantics；
- transport capabilities registry。

snapshot 不包含：

- listening socket；
- HTTP connection；
- SQLx pool；
- Moka store；
- writer channel 的发送端实现细节；
- mutable retry/backoff state。

`DnsService::with_default_timeout_from_runtime` 从 active snapshot 取得 `DnsCore` handle，确保 service task 与请求入口使用同一 `RuntimeRevision`。正式 `run` 路径由 `Application` 创建 `Arc<RuntimeCoordinator>`，再通过 `DnsService::with_default_timeout_from_coordinator` 共享 coordinator；service 启动时固定一份 listener/runtime 句柄给 transport task，但显式 `reload_prepared` 会按 BindPlan 选择重绑或复用 listener，在 revision CAS 激活后注册新 task 并取消旧 scoped token；resource refresh task 也按 active runtime 的 worker ID 集合使用独立 scoped token，在 reload 时取消旧集合并注册新集合。`PreparedRuntime` 在候选构造阶段装配生产 `ReqwestResourceFetcher`，并由 `ActiveRuntime` 持有其 shared adapter；该 adapter 不进入请求 snapshot。对三类 `auto_update=true` 资源，service 会把按 due/backoff 执行的 refresh worker 注册进同一 `Supervisor`，成功候选在当前 ActiveRuntime 内经 Policy 版本 CAS 和 Runtime 元数据 CAS 生效；资源内容刷新不改变 Runtime revision 或 listener，配置变化才创建候选 Runtime。`RuntimeCoordinator` current-target cell 让旧 Runtime 的 optimistic refresh/late sink 路由到最新 cache/finalizer，并由统一 owner 在 shutdown 时回收；进程持有的 database/logs/webui/resolve-log 配置不能原位替换，Application 文件 reload 会在 async prepare 前返回 `RestartRequired`，service 入口在激活前保留同一防线并继续使用旧 Runtime；完整 flush task 仍待完成。

每个请求在 ingress 后只捕获一次 `Arc<RuntimeSnapshot>`。同一请求不能在策略、缓存和上游阶段分别读取不同 revision。

## 4. PreparedRuntime 构建

prepare 按依赖顺序执行：

1. 接收 `ResolvedConfig` 和各类 prepare plan；
2. 打开必需业务数据库并执行 migration/可写性检查；
3. 创建 storage、telemetry、cache 等 shared service；
4. 创建远程资源首次加载所需的 outbound/connector；
5. 对 remote rule-set 恢复已校验的落盘 pair，必要时下载、解析并原子持久化，再编译所有首次 resource snapshot；
6. 编译 client、strategy、route 和 upstream registry；
7. 创建 transport profile 和 bind plan；
8. 执行跨模块 preflight；
9. 生成候选 revision。

任一步失败都销毁候选资源，不改变当前 ActiveRuntime。首启时失败直接返回 Application。

## 5. BindPlan

`BindPlan` 是经过 Config 校验后的显式 endpoint 列表，记录：

- listener/endpoint ID；
- 底层 UDP/TCP socket 协议、应用层 transport 类型和 bind address；
- IPv6 v6-only 选择；
- transport profile；
- TLS/client IP profile 的引用；
- accept task 的故障策略。

绑定采用“全部成功再提交”：

1. 逐项通过 `SocketFactory` 创建未激活 socket；
2. 若任一失败，关闭本轮已创建 socket；
3. 全部成功后组合为 `BoundListenerSet`；
4. 激活前不启动 accept loop；
5. ActiveRuntime 原子发布后统一启动或放行 accept。

v1 首次启动不存在端口复用迁移。未来 rebind 只有平台允许并且所有新 endpoint 成功时才切换。

## 6. Coordinator 与 CAS

`RuntimeCoordinator` 串行处理配置候选、资源更新和 fatal 状态迁移。Application 的配置 fingerprint 轮询只负责产生 reload 触发，不绕过 coordinator；资源刷新可以并行执行，但发布必须满足：

- 每个资源有独立 epoch；
- 结果 epoch 小于当前值时丢弃；
- 发布时基于最新 ActiveRuntime revision 合并 registry；
- CAS 失败后重新读取最新 registry 并重放本资源变更；
- 不从旧 registry 构造完整替换，避免并发更新互相覆盖。

当前 `RuntimeCoordinator` 已提供资源 worker 查询、刷新、关闭代理，以及把已 prepare 的候选通过 `bind_prepared` 和 revision CAS 激活的 `bind_and_activate` 入口；`refresh_resource_if_current` 在刷新前后校验 captured `ActiveRuntime` 仍是 coordinator 当前实例，stale 时由 service 跳过本轮并重新读取。Application 的 `reload_runtime_from_path` 已把无 snapshot 配置加载、SecretRef 校验、async prepare 和候选激活串成显式触发 API，`reload_service_from_path` 进一步调用 service listener/resource task swap；当 BindPlan 未变化时，coordinator 通过 Arc 复用当前已激活 listener，先执行 `PreparedRuntime::merge_state_from` 迁移兼容资源状态，避免同端口重复 bind，BindPlan 变化时仍要求新 listener 全部 bind 成功，任一 bind 失败都不会切换 revision 或停止旧 listener。每次 Runtime 成功发布时 coordinator 登记其 `LateCacheFinalizer` owner，service shutdown 在统一 deadline 内关闭历史与当前 owner；旧 Runtime owner 淘汰时同步清理无活动 finalizer，仍有活动 late task 的 owner 保留。`run` 通过配置 fingerprint 两次稳定轮询复用该入口，失败不会切换当前 runtime。当前资源刷新仍在该 runtime 内完成 Policy/metadata CAS。资源-only 更新复用现有 `BoundListenerSet` 和 `SharedServices`。需要 rebind 的候选拥有独立 listener set；发布后旧 runtime 进入 drain，旧 transport 和 resource task 通过 scoped cancellation 退出。

## 7. Supervisor

supervisor 持有完整 task tree：

```text
Supervisor
  ├─ listener groups
  │   ├─ UDP receive loops
  │   ├─ TCP accept task（内部持有 connection JoinSet）
  │   └─ DoH accept/connections
  ├─ resource refresh workers
  ├─ stats writer
  ├─ resolve-log writer
  ├─ cache persistence writer
  └─ telemetry flush worker
```

禁止使用无人持有的 `tokio::spawn`。每个 task 注册：

- task ID 与组件；
- cancellation token；
- restart policy；
- fatal/degraded 分类；
- 最近启动、失败和重试时间；
- shutdown hook。

`Supervisor::spawn` 继续接收一次性 `TaskFuture`，用于已经由上层持有重建逻辑的 task。需要 supervisor 自己重建的 task 使用 `spawn_with_factory`：factory 每次尝试生成一个新 future，只有 `TaskError::Transient` 且未收到 shutdown cancellation 时才按 `RestartPolicy::Transient { max_restarts }` 有界重试，并在重试间使用可取消的指数退避。需要在 reload 时单独停止的 task 使用 `spawn_scoped`，其 cancellation 仍由 supervisor 持有，并在全局 shutdown 时统一回收；需要同时支持两者的 task 使用 `spawn_scoped_with_factory`。Supervisor 同时记录 Tokio `JoinSet` task ID 到完整 FluxDNS `TaskSpec` 的映射，`JoinError` 的 panic/abort 结果按真实 task 归因并保留 component/fault level，不使用注册表顺序猜测兄弟 task。最终 `TaskCompletion` 携带 `restart_count`，达到上限的瞬时失败可由 `restart_exhausted()` 明确识别；`ShutdownReport` 聚合已发生的重试次数。当前 transport listener task 使用 `FatalEndpoint` 与三次有界瞬时重试，resource task 保持 `RestartPolicy::Never` 并在自身循环内 backoff；resource refresh 的启动、成功、失败和 worker 缺失会沿 telemetry health sink 发布低基数状态；显式 service reload 已使用 revision 化 task ID 注册新 listener，并取消旧 token；`DnsService` 已在等待 Ctrl-C 时观察 task completion，Degraded 终止只记录，FatalEndpoint/Fatal、重试耗尽和 panic 会升级为 `ServiceError::TaskFailure`。该重试只复用现有已绑定 socket，不等同于自动 rebind；listener factory 重建和完整 endpoint 故障矩阵仍留在后续阶段。

## 8. 故障处理

故障等级：

- `request-local`：单请求解析、timeout、取消；
- `degraded`：资源刷新、详情 writer、cache persistence、telemetry/log writer 等可降级故障；
- `fatal-candidate`：候选 prepare/bind 失败，保留旧 runtime；
- `fatal-endpoint`：单 endpoint 达到重试上限；
- `fatal`：逻辑 listener 全部不可用、supervisor panic、启动必需 storage 失效或 shutdown timeout。

聚合统计数据库故障最初按 degraded 处理；pending batch 和补偿计数使用 v1 固定内存保护上限。达到上限时升级 fatal，停止接收新请求并在有限 flush 后退出，避免长期故障演变为 OOM。

瞬时重试采用带 jitter 的指数退避并有上限。只有设计明确为瞬时的 task 可自动重启；逻辑错误、schema 错误和 panic 不无限重启。

## 9. 请求计数与 drain

每个 ActiveRuntime 持有请求 guard 计数：

- accept 新请求时创建 guard；
- response 完成、客户端断开或取消时释放；
- 进入 drain 后拒绝新 guard；
- guard 归零即完成 drain；
- grace deadline 到期后取消剩余请求并记录数量。

UDP 无连接请求同样受 guard 约束。后台 cache finalizer 如果已脱离客户端响应，但仍在允许窗口内，计入 runtime 后台 guard。

## 10. Shutdown

顺序固定：

1. `RuntimeCoordinator::begin_drain` 标记当前及历史 Runtime draining；
2. supervisor cancellation 停止 accept/receive 新请求；
3. TCP listener 取消并 join 内部 connection `JoinSet`；
4. supervisor 确认当前 task tree 清空；
5. `RuntimeCoordinator` 在 grace deadline 内等待当前及旧 Runtime 的请求 guard 归零；
6. 由 `RuntimeCoordinator` 统一登记的各 Runtime `PolicyDnsCore` owner 在同一 grace deadline 内关闭 `LateCacheFinalizer`，排空 cache persistence writer，并合并安全停机计数。

stats、resolve log、SQLite checkpoint 和 `TelemetryWriter` flush 已由 `StorageRuntime`/`DnsService` 接线并纳入 drain shutdown；production async prepare 也会在 bind 前完成 cache SQLite 恢复，并把 persistence writer 交给现有 finalizer owner 在同一 deadline 内有序关闭。finalizer 摘要会在 Telemetry 关闭前发布，未完成关闭才标记 shutdown deadline，失败或丢弃批次保持 degraded gap 语义。

每一步有独立 deadline 和结果，最终报告不能只返回模糊的“shutdown failed”。

## 11. 测试

- prepare 任一步失败都不发布 candidate；
- 全部 bind 成功前没有 endpoint 接收请求；
- CAS 竞争不会丢失两个不同资源更新；
- 同一请求始终使用一个 runtime revision；
- 资源-only 更新复用 listener；
- rebind 失败保留旧 runtime；
- endpoint 重试、degraded、fatal 升级符合矩阵；
- drain 等待、deadline 取消和 flush 顺序可确定性验证；
- task panic 被 supervisor 捕获且没有 detached task。

## 12. 实现检查清单

- [x] 定义 `RuntimeSnapshot`、`PreparedRuntime` 和无 socket preflight；
- [x] 以 `Arc<ResolvedConfig>` 作为 Config → Runtime 输入，拒绝空/重复 bind endpoint；
- [x] 实现基于 `SocketFactory` 的 BindPlan 全成/全退和 `v6_only` 规格传递；
- [x] 实现 `ArcSwap` ActiveRuntime coordinator/CAS、旧实例 draining 和请求 guard；
- [x] 实现 `Supervisor` task tree 基础、退出分类和受控 shutdown 回收报告；
- [x] 实现 UDP/TCP 不透明 socket capability、`SystemSocketFactory` 和 `BoundListenerSet` 句柄交接；
- [x] 接入真实 UDP/TCP service task、TCP session `JoinSet` 和基础 drain/shutdown；
- [x] 接入按配置生成的 immutable resource snapshot 摘要，并让 service 从 active snapshot 捕获同 revision `DnsCore`；
- [x] 接入 `RuntimeCoordinator` 级共享 `PolicyDnsCore` finalizer owner，在 service shutdown 中按统一 deadline 关闭历史与当前 Runtime owner，并合并 persistence 摘要；
- [x] 在 `PreparedRuntime`/`ActiveRuntime` 持有生产 `ResourceFetcher`，不把 HTTP client 放入请求 snapshot；
- [x] async prepare 在 bind 前完成 remote rule-set restore-or-fetch，并把 compiled snapshot 交给 Policy 初始构造；
- [x] 为 `auto_update=true` 的 remote/file rule-set 与 file hosts 注册 Supervisor refresh task，并在当前 ActiveRuntime 内执行 Policy/Runtime metadata live publish；
- [x] 由 `Application` 与 `DnsService` 共享持有 `RuntimeCoordinator`，资源 refresh task 通过 coordinator 读取当前活动 runtime；监听器 task 仍固定在启动时实例；
- [x] `Supervisor::spawn_with_factory` 实现瞬时失败的可取消指数退避、有界重试、重试次数和上限耗尽识别；当前 service task 的具体故障策略接入仍待完成；
- [x] `DnsService` 观察 Supervisor task completion，区分 Degraded 终止与 FatalEndpoint/Fatal、重试耗尽和 panic；listener 故障自动 factory 重建仍待完成；
- [x] 运行期 fatal/panic 返回前通过 `RuntimeCoordinator::begin_drain` 统一标记当前及历史 Runtime；
- [x] `Supervisor::spawn_scoped` 提供 task-scoped cancellation，且全局 shutdown 仍能回收 scoped task；DnsService 的 UDP/TCP/DoH listener 已使用独立 scoped token；
- [x] `Supervisor::spawn_scoped_with_factory` 提供带独立 cancellation 的有界瞬时重试；transport listener task 已接入三次重试上限，重试耗尽仍按 `FatalEndpoint` 升级；
- [x] Supervisor 通过 Tokio `JoinSet` task ID 映射精确归因 panic/abort 任务，避免按注册表顺序误删 sibling task；
- [x] `DnsService::reload_prepared` 在候选激活后重建 UDP/TCP/DoH listener 和 resource refresh task，并取消旧 scoped token；候选 bind 失败时旧 Runtime、task 和 listener 保持可用，bind plan 不变时复用原 listener 且后续请求读取新 Policy；
- [x] Application 文件 reload 在 async prepare 前、`DnsService::reload_prepared` 在激活前对进程持有配置变化返回 `RestartRequired`，保留旧 revision；
- [x] `RuntimeCoordinator::bind_and_activate` 接入候选 `PreparedRuntime → bind_prepared → revision CAS`，bind/CAS 失败保留旧 runtime 或返还可重试 candidate；Application 已提供显式配置文件 reload 触发 API，service-aware reload 可重建 listener task；
- [x] `RuntimeCoordinator::refresh_resource_if_current` 以 captured runtime 做前后活动实例校验，stale 时返回显式 coordinator error；service 重新读取当前 runtime 后再尝试；
- [x] 候选 revision CAS 与资源刷新共用 mutation gate，并迁移兼容资源的 Policy、metadata 和 worker 稳定调度状态；in-flight reservation 不跨 Runtime 复制；
- [x] 当前及旧 Runtime 均提供基于 `Notify` 的 deadline-aware request drain wait，并由 `RuntimeCoordinator`/`DnsService::shutdown` 接线；
- [x] `RuntimeCoordinator::begin_drain` 统一标记当前及历史 Runtime 为 draining，shutdown 不依赖可能过期的 service runtime 句柄；
- [x] `RuntimeCoordinator` 在新 Runtime 激活时惰性清理无活动请求的旧 draining owner，并同步清理无活动 `LateCacheFinalizer` owner；仍被 Runtime owner 保留或存在活动 late task 的 owner 继续保留直到 drain；
- [x] 定义状态类型与所有权转换；
- [x] 完成跨模块资源装配版 PreparedRuntime/preflight；
- [x] 完成真实服务任务版 ActiveRuntime coordinator/CAS 与 reload；
- [x] 完成 supervisor 故障升级、重启和分项 shutdown 报告；
- [ ] 完成完整 drain/shutdown（flush、checkpoint、超时分项报告）；
- [ ] 完成并发、故障和时间控制测试。

阶段证据：`runtime::prepared::tests` 验证 async prepare 的 remote restore/fetch、file snapshot load、持久化和 refresh worker 构造；定向测试额外验证 cache persistence 在 bind 前完成 recovery/owner 接线并可有序 shutdown；`runtime::coordinator::tests` 当前 16 项通过，覆盖资源刷新代理、stale-active guard、候选 bind/activate、revision CAS、Runtime drain、finalizer owner 清理与停机摘要合并；`app::tests` 当前 11 项通过，新增用例验证直接 Runtime reload 在 prepare 前拒绝进程持有配置变化；`runtime::supervisor::tests` 覆盖有界重试、scoped cancellation、shutdown report 和 panic 归因；`service::tests::` 聚焦筛选当前 37 项通过，其中主 Service 24 项，覆盖 listener reload/reuse、bind plan 不变时同一地址读取新 Policy answer、候选 bind 失败保留旧 UDP 请求路径、resource task、故障升级、统一 drain、live resource publish、跨 transport 一致性，以及进程级配置变化时返回 `RestartRequired` 且旧 Runtime 不切换。资源热更新的 Service/Runtime 两项关键测试已复核通过；完整跨 Runtime shared-service 与 flush 生命周期仍未完成。

当前实现进度：**80%**。已验证 Runtime snapshot 资源摘要、原子资源 metadata publish、service core 构造入口、生产 ResourceFetcher ownership、production async cache recovery/persistence owner、RuntimeCoordinator 级历史/当前 Policy finalizer owner、无活动旧 finalizer owner 的淘汰清理、同一 ActiveRuntime 内的 remote/file refresh worker/CAS publish、候选 registry 的更高版本合并原语、候选 revision CAS 下的 Policy/worker/metadata 合并、配置 watcher 的 service-aware reload、进程持有配置早期拒绝、listener 重建/复用、Policy answer 切换及 bind 失败回滚、resource worker 按 ID 增量复用/移除、transport listener task 的 scoped 有界瞬时重试、当前与旧 Runtime 的 deadline-aware request drain wait、新激活时无活动旧 draining owner 的惰性清理、当前/历史 Runtime 的统一 begin_drain，以及运行期 fatal/panic 路径的 coordinator drain。资源内容刷新固定使用同 Runtime per-resource CAS；完整跨 Runtime shared-service 生命周期、listener factory 自动重建和完整服务级故障矩阵仍未接线。
