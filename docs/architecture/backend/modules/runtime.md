# Runtime 模块设计

> 文档状态：有效
>
> 适用范围：Runtime 状态、listener 生命周期、后台任务监督、原子激活和 shutdown
>
> 最后评审：待核对（本次仅分类与边界复核，不等同完整契约重审）
>
> 关联实现：`backend/src/runtime/*`
>
> 关联文档：[后端架构](../overview.md)

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

service task 与请求入口必须使用同一 RuntimeRevision；transport 持有固定 runtime 句柄，显式 reload 才换代。resource worker 通过 coordinator 查询当前实例；Storage/Telemetry 按进程复用；历史和当前 finalizer owner 都受停机管理。Management listener 与用户 reload 边界遵循[管理面设计](../../management.md)，正式构造与调用顺序见[生命周期实现](../../../implementation/backend/lifecycle.md)。

每个请求在 ingress 后只捕获一次 `Arc<RuntimeSnapshot>`。同一请求不能在策略、缓存和上游阶段分别读取不同 revision。

## 4. PreparedRuntime 构建

启动准备必须在 bind 前满足以下前置条件；这些职责由 Application、PreparedRuntime 和进程服务共同承担，不表示全部位于同一个构造器或严格按本表顺序执行：

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

stats、resolve log、SQLite checkpoint 和 Telemetry flush 都必须纳入进程 drain。cache persistence 由 finalizer owner 有序关闭，安全摘要在 Telemetry 关闭前发布；未完成关闭与持久化失败/drop 分别记录，不能混成同一 timeout。实际 owner 和 task 接线见[后台服务](../../../implementation/backend/background-services.md)。

每一步有独立 deadline 和结果，最终报告不能只返回模糊的“shutdown failed”。

## 11. 契约验证要求

- prepare 任一步失败都不发布 candidate；
- 全部 bind 成功前没有 endpoint 接收请求；
- CAS 竞争不会丢失两个不同资源更新；
- 同一请求始终使用一个 runtime revision；
- 资源-only 更新复用 listener；
- rebind 失败保留旧 runtime；
- endpoint 重试、degraded、fatal 升级符合矩阵；
- drain 等待、deadline 取消和 flush 顺序可确定性验证；
- task panic 被 supervisor 捕获且没有 detached task。
