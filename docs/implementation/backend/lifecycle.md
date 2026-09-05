# 后端生命周期实现

> 文档状态：有效
>
> 适用范围：正式 CLI 启动、资源准备、bind、reload 与 shutdown 接线
>
> 最后核对：2026-09-05（入口与所有权静态追踪）
>
> 核对基线：`0f18d5b2ddf67625121fd7e0662e21723362565f`

## 正式入口

[`main.rs`](../../../backend/src/main.rs) 的异步 `main` 初始化 bootstrap telemetry 后调用 [`app.rs`](../../../backend/src/app.rs) 的 `run` / `run_with_args` / `run_command`。CLI 由 `parse_args` 解析，`run` 与 `validate` 共用严格配置加载；未指定 CLI 配置时使用 `config.yaml`。这与要求显式 `-ConfigPath` 的[开发服务脚本](../delivery.md)不是同一参数边界。

`run_command` 的顺序是：

1. 仅 `run` 先执行 `recover_pending_transaction`；`validate` 关闭 snapshot 写入，不恢复写事务。
2. `ConfigLoader::load_from_path` 加载配置；`run` 再解析检查 SecretRef 并配置正式日志输出。
3. 调用 `PreparedRuntime::prepare_with_policy_core_and_remote_resources`，准备资源、Policy core、upstream 和启用的缓存恢复。
4. `StorageRuntime::open` 打开统计/详情数据库并迁移；失败映射为 prepare 错误。
5. `bind_prepared` 使用 `SystemSocketFactory` 绑定 DNS endpoint，构造 `RuntimeCoordinator`。
6. WebUI 启用时 `ManagementService::bind` 注入 coordinator、数据库路径、详情开关、telemetry 与 resolution metrics。
7. 构造 `DnsService`，通过 `attach_management` 注册管理服务；进入信号、Supervisor 和配置 watcher 等待。

`validate` 不执行后面的资源网络 fetch、数据库打开或 listener bind，因此配置校验通过不证明端口、秘密实际值、资源、SQLite 或网络可用。

## 候选与活动实例

[`prepared.rs`](../../../backend/src/runtime/prepared.rs) 的 async prepare 先检查本地/远程 snapshot；remote rule-set 优先恢复已验证 content/manifest，无有效恢复才 bounded fetch、解析与持久化。构造 `PolicyDnsCore` 后，如缓存启用则初始化独立 SQLite cache persistence；失败降级 warning，不等同统计数据库失败。

[`coordinator.rs`](../../../backend/src/runtime/coordinator.rs) 保存活动 runtime 和历史 finalizer owner；[`bind.rs`](../../../backend/src/runtime/bind.rs) 与 [`system_socket.rs`](../../../backend/src/runtime/system_socket.rs) 负责真实绑定、TLS 材料和系统 socket。`PreparedRuntime` 不是已监听的服务，候选失败不会改变旧活动实例。

## Reload

[`app.rs`](../../../backend/src/app.rs) 的 `prepare_reload_candidate`、`reload_runtime_from_path` 与 `reload_service_from_path` 重读配置时关闭 snapshot 写入，先拒绝进程持有配置的变化，再 prepare。正式 service watcher 调用 service-aware 入口，不只替换裸 coordinator。

[`DnsService::reload_prepared`](../../../backend/src/service.rs) 按 BindPlan 复用未变 listener 或先绑定新入口，经 revision CAS 激活后重建 transport/resource task，并取消旧 scoped token。候选失败保留旧 runtime；watcher 只在成功后提交 fingerprint。

Storage/Telemetry 和解析统计 sink 由进程持有，reload 为候选 core 复用这些 sink。`webui.users` 激活后交给 `ManagementRuntime::reconcile_users`，内部写入识别与外部 session 撤销见[管理端](management.md)。其他 restart-required 字段见[配置参考](../configuration.md)。

## Shutdown 与错误

[`service.rs`](../../../backend/src/service.rs) 的 `wait_for_termination_signal` 覆盖 Ctrl-C 和 Unix `SIGTERM`；`shutdown` 先停止 Management，再取消任务、标记 drain、等待连接/请求、关闭历史与当前 finalizer、flush 进程服务。阶段共用 deadline，错误保留已完成阶段报告；第二终止信号可快速结束等待。

入口和 task 错误由 `AppErrorKind` / `ServiceError` 分类，不能把 runtime fatal 或超时映射为成功。详细设计见 [Application](../../architecture/backend/modules/application.md) 与 [Runtime](../../architecture/backend/modules/runtime.md)。

## 能力与证据

| 能力 | 代码实现 | 正式入口接线 | 验证证据 | 已知限制 |
| --- | --- | --- | --- | --- |
| 完整启动 | `run_command`、async prepare、bind、StorageRuntime | `main -> app -> DnsService` | 本轮静态追踪；源码有 `async_prepare_attaches_cache_persistence_before_bind` 等测试，本轮未运行 | 不证明环境资源/端口可用 |
| 配置切换 | `reload_service_from_path`、`reload_prepared` | watcher 调用，复用进程服务 | 本轮核对调用点；存在 reload/rebind/failure 测试 | 不宣称所有平台组合已验收 |
| 有界停机 | `shutdown`、signal wait、finalizer owner | 正常信号及 fatal task 路径 | 本轮核对分支；存在 shutdown timeout/flush 测试 | Unix 双信号真实进程 smoke 尚无本轮证据 |
| 安全 panic hook | 设计要求仅输出安全信息 | 未找到 `std::panic::set_hook` 的进程接线 | 本轮搜索入口与源树 | 不能据设计宣称默认 panic 输出已脱敏，见[差距计划](../../plans/backend-contract-gaps.md) |

本轮未执行 Cargo 测试、端口绑定、真实信号或服务 smoke；测试符号只用于定位覆盖，不是通过记录。
