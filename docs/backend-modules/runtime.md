# Runtime 模块设计

> 状态：v1 方案已完成，阶段 3 基础服务编排、Runtime 资源摘要和 service core 构造入口已实现，完整资源/flush 生命周期仍在后续阶段
>
> 更新日期：2026-09-01
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
- per-resource registry snapshot（当前 Runtime-facing 版本为按配置生成的元数据摘要，compiled payload 仍由 `PolicyDnsCore` 持有）；
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

`DnsService::with_default_timeout_from_runtime` 从 active snapshot 取得 `DnsCore` handle，确保 service task 与请求入口使用同一 `RuntimeRevision`。当前尚未把资源 refresh worker、资源级 CAS reload、resource-only runtime swap 或 flush task 接入该 snapshot。

每个请求在 ingress 后只捕获一次 `Arc<RuntimeSnapshot>`。同一请求不能在策略、缓存和上游阶段分别读取不同 revision。

## 4. PreparedRuntime 构建

prepare 按依赖顺序执行：

1. 接收 `ResolvedConfig` 和各类 prepare plan；
2. 打开必需业务数据库并执行 migration/可写性检查；
3. 创建 storage、telemetry、cache 等 shared service；
4. 创建远程资源首次加载所需的 outbound/connector；
5. 加载并编译所有首次 resource snapshot；
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

`RuntimeCoordinator` 串行处理配置候选、资源更新和 fatal 状态迁移。资源刷新可以并行执行，但发布必须满足：

- 每个资源有独立 epoch；
- 结果 epoch 小于当前值时丢弃；
- 发布时基于最新 ActiveRuntime revision 合并 registry；
- CAS 失败后重新读取最新 registry 并重放本资源变更；
- 不从旧 registry 构造完整替换，避免并发更新互相覆盖。

资源-only 更新复用现有 `BoundListenerSet` 和 `SharedServices`。需要 rebind 的候选拥有独立 listener set；发布后旧 runtime 进入 drain。

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

1. `DnsService::shutdown` 标记 runtime draining；
2. supervisor cancellation 停止 accept/receive 新请求；
3. TCP listener 取消并 join 内部 connection `JoinSet`；
4. 在 grace deadline 内等待存量请求；
5. supervisor 确认当前 task tree 清空。
6. 由当前 runtime snapshot 的 `PolicyDnsCore` owner 在同一 grace deadline 内关闭 `LateCacheFinalizer`。

stats、resolve log、cache persistence、SQLite checkpoint 和 telemetry flush 尚未接线。

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
- [x] 接入当前 snapshot `PolicyDnsCore` finalizer owner，并在 service shutdown 中执行 deadline-aware close；共享 Runtime 后台 owner 仍待完成；
- [ ] 定义状态类型与所有权转换；
- [ ] 完成跨模块资源装配版 PreparedRuntime/preflight；
- [ ] 完成真实服务任务版 ActiveRuntime coordinator/CAS 与 reload；
- [ ] 完成 supervisor 故障升级、重启和分项 shutdown 报告；
- [ ] 完成完整 drain/shutdown（flush、checkpoint、超时分项报告）；
- [ ] 完成并发、故障和时间控制测试。

当前实现进度：**35%**。已验证 Runtime snapshot 资源摘要、service core 构造入口和当前 Policy finalizer owner；完整资源 worker、reload/CAS 合并、flush 和故障注入仍未接线。
