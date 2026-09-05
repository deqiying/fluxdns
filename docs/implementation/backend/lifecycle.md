# 后端生命周期实现

> 文档状态：有效
>
> 适用范围：正式 CLI 启动、资源准备、bind、reload 与 shutdown 接线
>
> 最后核对：2026-09-05（UTC；启动/停机、刷新/reload 竞争与 owner 本机契约验证）
>
> 核对基线：`f65fb3f8bd68e1a40ca041d9a380859b44a3da0c` 加本次契约验证工作树

## 正式入口

[`main.rs`](../../../backend/src/main.rs) 的异步 `main` 先安装 [`panic_safety`](../../../backend/src/panic_safety.rs) hook，再初始化 bootstrap telemetry 并调用 [`app.rs`](../../../backend/src/app.rs) 的 `run` / `run_with_args` / `run_command`。CLI 由 `parse_args` 解析，`run` 与 `validate` 共用严格配置加载；未指定 CLI 配置时使用 `config.yaml`。这与要求显式 `-ConfigPath` 的[开发服务脚本](../delivery.md)不是同一参数边界。

`run_command` 的顺序是：

1. 仅 `run` 先执行 `recover_pending_transaction`；`validate` 关闭 snapshot 写入，不恢复写事务。
2. `ConfigLoader::load_from_path` 加载配置；`run` 再解析检查 SecretRef 并配置正式日志输出。
3. 调用 `PreparedRuntime::prepare_with_policy_core_and_remote_resources`，准备资源、Policy core、upstream 和启用的缓存恢复。
4. `StorageRuntime::open` 在共享 deadline 内建目录、打开统计/详情数据库、迁移并执行独立事务写入/回滚探针；失败映射为 prepare 错误，不创建服务 owner。
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

TelemetrySampler 的 Resolution metrics Source Arc 和采样游标同样属于进程 owner，reload 不重置累计量。与之不同，重新 prepare 的 DoH connector 创建独立 bootstrap 地址缓存；旧请求只能填旧 resolver，候选失败不影响活动缓存。资源-only publish 未替换 connector 时继续使用其原缓存。

## Shutdown 与错误

[`service.rs`](../../../backend/src/service.rs) 的 `wait_for_termination_signal` 覆盖 Ctrl-C 和 Unix `SIGTERM`；`shutdown` 先停止 Management，再标记当前/历史 runtime draining、取消任务、回收 Supervisor 并等待请求；随后关闭 Resolution、历史/当前 finalizer、Storage，最后关闭 Telemetry。阶段共用 deadline，错误保留已完成阶段报告；第二终止信号可快速结束等待。

TCP/DoH 和 UDP dispatch 都可能被 service cancellation 中止，因此 5 秒 grace 是回收总预算，不等于保证已读请求一定响应。`RuntimeSnapshot` 持有 core，而内部 Resolution/Storage detail/finalizer task 分别由 owner 管理；不能把 Supervisor 的单独 drain 视为全部后台服务已关闭。

Storage 停机先关闭 detail 输入并回收当前正在提交的 batch，不预先排空其余队列；然后提交 stats，详情使用同一 deadline 的剩余时间，最后关闭数据库。正在执行的 SQL 可能消耗预算，超时按失败报告，不承诺强制抢占 SQL 或无损停机。

最后关闭 Telemetry 前再次采样已回收 Resolution owner 的 accepted 与事件队列深度，复用周期游标；再关闭 writer 输入、排空事件并输出最终累计快照。采样失败也必须关闭 writer，并将错误保留到 shutdown report，不延长总预算。

入口和 task 错误由 `AppErrorKind` / `ServiceError` 分类，不能把 runtime fatal 或超时映射为成功。详细设计见 [Application](../../architecture/backend/modules/application.md) 与 [Runtime](../../architecture/backend/modules/runtime.md)。

## 能力与证据

| 能力 | 代码实现 | 正式入口接线 | 验证证据 | 已知限制 |
| --- | --- | --- | --- | --- |
| 完整启动 | `run_command`、async prepare、StorageRuntime deadline/probe | `main -> app -> DnsService` | 过期预算不建库、真实 SQLite 写锁与写入拒绝/回滚测试 | 不证明真实磁盘满或权限故障全部可恢复 |
| 配置切换 | `reload_service_from_path`、`reload_prepared` | watcher 调用，复用进程服务 | 完整测试包含 reload/rebind/failure 用例 | 不宣称所有平台组合已验收 |
| 有界停机 | `shutdown`、finalizer owner、stats-first | 正常信号及 fatal task 路径 | SQLite trigger 验证 stats 提交先于 300 条多批详情排空 | 已执行 SQL 无强制抢占保证；Unix 双信号 smoke 未执行 |
| 安全 panic hook | 固定分类、受限源码位置、backtrace 状态 | 异步 `main` 第一项安装 `std::panic::set_hook` | 独立子进程验证主线程/worker panic 不泄漏 payload、线程名或完整栈 | 不改变内部 task owner 的失败升级策略；安装前异常不覆盖 |

2026-09-05 的完整 Cargo 测试与 loopback 验证见[后台服务](background-services.md#本次验证)；没有真实 Unix 信号或外部部署 smoke 证据。

## 契约验证补充

以下是 `f65fb3f8bd68e1a40ca041d9a380859b44a3da0c` 之后工作树的测试实现；运行结果与可重复入口统一见[契约验证运行入口](background-services.md#契约验证运行入口)。不覆盖真实 Unix 进程信号，也不把内部 worker 回收等同于 Supervisor 的失败升级。

| 用例 | 入口与同步点 | 断言与证据边界 |
| --- | --- | --- |
| V1-P01 | `prepared::tests::contract_v1_split_publication_keeps_resources_and_generations_isolated`；在 Policy 发布后用实例级 `TestGate` 暂停 metadata | 观察合法分步状态，叠加另一资源更新和新 candidate 激活，旧 metadata 仅更新旧 snapshot；这是 `ActiveRuntime::refresh_resource` / `activate` 底层交错，不冒充 service 串行 mutation 的整条 reload 流程 |
| V1-P02 | `snapshot::tests::contract_v1_concurrent_metadata_cas_keeps_both_resources_monotonic`；两个 worker 在 32 轮 barrier 后发布 | 两个资源均保留最高版本，同资源迟到版本不回退，runtime revision 不变 |
| V1-R01 | `service::tests::contract_v1_service_reload_serializes_refresh_and_reclaims_both_owners`；真实 UDP service、file refresh 与 Policy/metadata 同步点 | 复用和 rebind 均等待旧刷新，合并已发布版本；20ms reload 不能越过原预算，超时/失败绑定保留旧 runtime，后续正常 reload 仍成功；当前/历史 finalizer panic 后 active 归零，最终停机关闭 owner 并释放端口 |
| V1-T01 | `supervisor::tests::contract_v1_retry_timer_is_separate_from_clock_and_cancellable` | `FakeClock` 推进不触发 Tokio retry；显式推进 Tokio timer 后进入第二次 backoff，scoped cancellation 结束任务且不取消 Supervisor |
| V1-T02 | `supervisor::tests::contract_v1_shutdown_deadline_is_owned_by_injected_clock` | Tokio 时间推进不耗尽注入 Clock 的停机预算；只推进该 Clock 至原 deadline，回收不合作任务并报告 abort |
| V1-O02 | `cache::runtime::tests::contract_v1_persistence_worker_panic_is_reclaimed_and_sanitized`；测试 store 在实际 persistence worker 调用中 panic | owner join 返回 `Internal / cache_persistence.worker`，关闭 channel、回收句柄，错误不包含 panic payload；不会自动重启 worker |
| V1-O03 | `storage::service::tests::contract_v1_detail_owner_panic_preserves_stats_and_safe_error` | 在真实 detail worker 返回后模拟 join panic，仍提交统计并关闭 backend；错误为 `Internal / sqlite_resolve_log.worker`，可重开数据库，不包含 payload |
| V1-O04 | `resolution::tests::contract_v1_resolution_owner_join_panics_report_incomplete` | 分别在 dispatcher/cache/detail worker 返回后模拟 join panic；shutdown 明确 `completed=false`，收齐三个句柄、停止 publisher，不把回收当作运行成功 |
| V2-O01 | `coordinator::tests::contract_v2_expired_shutdown_reclaims_unpolled_historical_and_current_tasks` | 当前与历史 finalizer 的任务尚未 poll 就被过期 deadline abort；报告未完成，但 active 必须归零、owner 关闭并拒绝新任务 |

`TestGate` 位于 [`ports/testing.rs`](../../../backend/src/ports/testing.rs)，只有 `cfg(test)` 构建存在；到达和放行用不同 Semaphore，观察同步点有 5 秒 watchdog。`PreparedRuntime` 的暂停点只属于测试实例，没有全局开关、生产配置或正式运行额外 await。Tokio `test-util` 仅在 dev-dependencies 中启用，暂停时间只用于纯调度测试；真实 socket、TLS 和 SQLite 用实时钟。

V2-O01 曾在修复前稳定复现 active 为 1 而非 0：[`LateCacheFinalizer::submit_task`](../../../backend/src/cache/service.rs) 原来在 async task 第一次 poll 时才构造 guard。现改为提交前构造、由 future 捕获；首次 poll 前 abort/drop 也释放计数和 semaphore permit。没有修改容量、响应时序、late-result 窗口、失败升级或停机 deadline。`wait_idle_for_test` 只等待现有 idle 通知，不通过取消任务伪造正常完成。

V1-R01 还复现了 service reload 等待 mutation gate 不受调用方 deadline 限制：20ms 预算一直等到 200ms 测试 watchdog。[`DnsService::reload_prepared`](../../../backend/src/service.rs) 现在在 listener 复用和 rebind 的 activation 等待外使用原 deadline，超时返回 `ServiceReloadError::Timeout`，不发布候选、不增加重试、不改变旧 runtime。此边界不承诺强行抢占已经进入 OS 的调用或同步 activation。

现有 serialized activation CAS、旧 scoped task 退出、Supervisor panic 归因以及进程 panic hook 测试由全量回归复用。上述 owner 用例区分 adapter panic 与 worker 返回后的 join panic，未引入统一自动恢复策略；真实 Unix 首/双信号及其在途组合仍按[活动计划](../../plans/backend-contract-validation.md)保留。
