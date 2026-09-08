# 后端生命周期实现

> 文档状态：有效
>
> 适用范围：正式 CLI 启动、资源准备、bind、reload 与 shutdown 接线
>
> 最后核对：2026-09-08（P1 配置 operation、文件观测与 service 应用接线）
>
> 核对基线：`4a5a5a7b13896f3b4c1d86fe4469a3afae38ac10` 加本次 P1 工作树

## 正式入口

[`main.rs`](../../../backend/src/main.rs) 的异步 `main` 先安装 [`panic_safety`](../../../backend/src/panic_safety.rs) hook，再初始化 bootstrap telemetry 并调用 [`app.rs`](../../../backend/src/app.rs) 的 `run` / `run_with_args` / `run_command`。CLI 由 `parse_args` 解析，`run` 与 `validate` 共用严格配置加载；未指定 CLI 配置时使用 `config.yaml`。这与要求显式 `-ConfigPath` 的[开发服务脚本](../delivery.md)不是同一参数边界。

`run_command` 的顺序是：

1. 仅 `run` 先执行 `recover_pending_transaction`；`validate` 关闭 snapshot 写入，不恢复写事务。
2. `ConfigV2Loader::load_from_path` 只加载 `version: 2`，`run` 先恢复已知配置文件事务并建立 active `ConfigStore`，再检查 SecretRef、配置正式日志输出并始终创建 telemetry writer 和日志 owner。
3. 调用 `PreparedRuntime::prepare_with_policy_core_and_remote_resources`，准备资源、Policy core 和 upstream；候选 prepare 不读写 cache snapshot。
4. `StorageRuntime::open` 在共享 deadline 内建目录、打开统计/详情数据库、迁移并执行独立事务写入/回滚探针；失败映射为 prepare 错误，不创建服务 owner。
5. app 创建唯一 `CacheSnapshotOwner`，在独立的有界 prepare deadline 内把快照分批恢复到活动 Moka；恢复故障降级冷启。owner 就绪后才继续 bind。
6. `bind_prepared` 使用 `SystemSocketFactory` 绑定 DNS endpoint，构造 `RuntimeCoordinator` 并登记与活动 core 匹配的 snapshot owner。
7. 构造 `DnsService` 并通过 `attach_logging` 挂接同一进程日志 owner，再取得有界 `ServiceControl`。
8. WebUI 启用时 `ManagementService::bind_with_config_store` 注入活动 ConfigStore、ServiceControl、coordinator、数据库/详情读口、telemetry 与 metrics，随后通过 `attach_management` 注册管理服务；最后进入信号、Supervisor 和配置 watcher 等待。

`validate` 不执行后面的资源网络 fetch、数据库打开或 listener bind，因此配置校验通过不证明端口、秘密实际值、资源、SQLite 或网络可用。

## 候选与活动实例

[`prepared.rs`](../../../backend/src/runtime/prepared.rs) 的 async prepare 先检查本地/远程 rule-set snapshot；remote rule-set 优先恢复已验证 content/manifest，无有效恢复才 bounded fetch、解析与持久化。构造 `PolicyDnsCore` 只建立 Moka source，不创建 SQLite cache persistence 或读写缓存文件。初次恢复由 app 的进程级 `CacheSnapshotOwner` 完成；reload 候选不恢复磁盘，避免准备态产生缓存副作用。

[`coordinator.rs`](../../../backend/src/runtime/coordinator.rs) 保存活动 runtime 和历史 finalizer owner；[`bind.rs`](../../../backend/src/runtime/bind.rs) 与 [`system_socket.rs`](../../../backend/src/runtime/system_socket.rs) 负责真实绑定、TLS 材料和系统 socket。`PreparedRuntime` 不是已监听的服务，候选失败不会改变旧活动实例。

## Reload

[`app.rs`](../../../backend/src/app.rs) 的 `prepare_reload_candidate`、`reload_runtime_from_path` 与 `reload_service_from_path` 是保留的显式内部入口：重读配置时关闭 snapshot 写入，先拒绝进程持有配置的变化，再 prepare。正式 app watcher 不再调用它们，只生成双文件观测提示。

[`DnsService::reload_prepared`](../../../backend/src/service.rs) 统一调用 `bind_prepared_reusing`，按物理 `SocketSpec` 复用未变句柄，只为新增或改变的 endpoint 创建 socket。transport/resource task 先注册并等待启动闸门，全部准备成功后执行 revision CAS，再同步更新服务集合并放行任务；旧 transport 由 Runtime retirement 通知停止接纳，已接纳请求继续完成，被移除的资源任务单独取消。该显式应用入口不由文件 watcher 触发。

cache snapshot 路径在候选发布前经过 owner 校验；Runtime CAS 成功后同步切换 owner 的 revision、Moka source 和 generation，不恢复候选、不等待磁盘写入。旧 generation 即使已经完成临时文件，也必须在最终替换前失去发布权。服务关闭时先停止 Resolution 并排空所有 cache finalizer，再用剩余总预算写当前 generation 的最终快照，最后关闭 Storage 与 Telemetry。

Storage/Telemetry 和解析统计 sink 由进程持有，reload 为候选 core 复用这些 sink。`webui.users` 显式激活后交给 `ManagementRuntime::reconcile_users`，内部写入识别与凭据变化撤销会话见[管理端](management.md)。其他 restart-required 字段见[配置参考](../configuration.md)。

TelemetrySampler 的 Resolution metrics Source Arc 和采样游标同样属于进程 owner，reload 不重置累计量。与之不同，重新 prepare 的 DoH connector 创建独立 bootstrap 地址缓存；旧请求只能填旧 resolver，候选失败不影响活动缓存。资源-only publish 未替换 connector 时继续使用其原缓存。

P1 的 logs enable/level/path 已通过 ConfigMutationOwner 进入 service-aware 热应用边界，先预开输出，再协调 filter/Runtime 发布，保留旧输出和真实失败分类，见[日志热切换](background-services.md#p1-日志热切换2026-09-07)。仅 coordinator 的旧入口没有日志 owner，继续拒绝日志变化。

### P1 仅提示文件观测（2026-09-07）

正式 `run_command` 使用加载结果中的绝对源路径与 `work.snapshot_path`，相同路径只观测一次。[`ConfigFileWatcher`](../../../backend/src/app/config_watcher.rs) 复用 ConfigStore 的 `ManagedObservation`，源/派生文件分别受 4 MiB、SHA-256、文件身份和路径链接检查约束；服务轮询只收取已结束的结果，文件读取移到最多一个在途 `spawn_blocking` 任务，不积压读取队列。

连续两次稳定观测后，watcher 先把同一份 `ManagedObservation` 写入活动 ConfigStore，再产生 `configuration_files_observed` 事件；Store 正忙时保留观测并在下一轮重投，不在 service loop 同步读盘。tracing 调用点带组合 revision、readable/missing/unreadable/oversized 状态和 `not_reloaded`，不带内容或凭据；既有 `TypedTracingLayer` 只保留固定日志字段，正式 JSON 目前保留事件名而不保留上述自定义观测字段。首次稳定状态也中性上报。不解析或应用磁盘候选、不更新会话、不修改磁盘；变更提示不等于候选有效。退出停止调度并有界等待只读任务；若无法确认完成则显式警告，不声称 OS 文件 I/O 已取消。

Windows 定向测试覆盖双文件、防抖、同长度内容变化、同内容文件身份替换、无效 YAML、缺失、非文件、超限和慢读取单在途。真实 UDP 服务在与生产相同的控制循环中，源/派生文件连续变化后保持同一个 Runtime、revision 和 DNS 策略；随后仅改 Hosts 资源文件，由正式 resource worker 到期刷新 DNS 结果，未手动调用 refresh。该 watcher 证据早于 BC-26，仍只证明外改不触发 reload；v2 冷启/重启证据单独记录。

BC-30 的 ConfigStore [还原](../configuration.md#p1-受管文件还原内部能力2026-09-07)、[外改确认重试](../configuration.md#p1-外改确认重试内部能力2026-09-07)、脱敏差异和状态/文件端点已正式接线；WebUI 全局提示见前端实现。文件系统停滞的强制中断和完整应用级 shutdown 总预算仍未验证。

### P1 差量 socket 子项（2026-09-07）

`BoundEndpoint` 使用共享的 `Arc<dyn ActivatedSocket>`。复用键是底层协议、绑定地址/端口、`reuse_port` 和 `v6_only`；listener 名称、策略和 DoH 路由不参与物理键。新集合保留候选的 `BindEntry`，不会因复用句柄而保留旧逻辑名称。新 socket 仍先全部 prepare 再 activate；候选释放只减少旧句柄引用，不关闭仍由活动实例持有的 socket。地址范围重叠但物理键不同的重绑仍可能被操作系统拒绝，不通过关闭旧端口或启用端口共享强行成功。

Windows Rust 1.98.0 定向证据：`runtime::bind::tests` 6 项、`service::tests::reload` 7 项通过；扩大到 `runtime::` 筛选回归 64 项通过（包含同名 cache runtime 测试），全部 Cargo 测试目标 `--no-run` 编译、fmt、文档及 diff 检查通过。新增 fake factory 用例核对改名复用、只准备变更端口、prepare/activate 失败后引用与释放计数；真实 loopback 用例仅改变 UDP 端口，确认 TCP/DoH 句柄 `Arc::ptr_eq`，在切换前后及占用新端口导致拒绝后执行 UDP/TCP/DoH POST/GET 查询，校验关联 ID、RCODE 和 canonical 响应一致。新用例不打开数据库、不加载个人配置，配置工作路径为 `_fluxdns/p1-service-differential`，端口由系统临时分配。

这只是 BC-03 已接入现有 service 的差量 socket 子项，不是完整 BC-03。后续任务预注册、请求 drain 和服务控制队列实现见下文；仍需新配置 owner 的真实补偿与完整应用成功边界。该批真实测试使用 v1 内存夹具，不证明后来接线的 v2 loader、HTTP/WS、持续无丢包热更新或日志切换组合已可用。

### P1 任务预注册子项（2026-09-07）

`RuntimeCoordinator::prepare_service_activation` 在既有 mutation gate 内合并资源状态并建立未发布实例；`ServiceActivation` 持有该 gate，丢弃准备态不会改变当前指针、admission 或 owner 登记。service 在此期间完成 transport 和新增资源任务注册，任务由同一 Supervisor 管理，`TaskStartGate` 仅复用 Tokio watch，不添加第二套 task tree。

启动闸门放行前不调用 transport factory，也不进行 receive/accept 或资源刷新。注册/版本/deadline 检查失败时，sender 释放，候选任务以 `Cancelled` 退出；旧任务集合和 admission 不变。部分成功注册的 task ID 在 Supervisor 回收 completion 前仍占用，不能重放操作绕过该约束。CAS 保留最终指针核对，不假定 mutation gate 能阻止所有底层非串行 API。成功后无 await 地更新服务集合、通知旧 transport 退场、取消被移除的资源任务、协调认证并放行新任务，消除了原先“先 CAS、再执行可失败 task 注册”的窗口。

Windows 定向验证包括真实 Supervisor 的 transport ID 冲突和 transport 注册成功后的 resource ID 冲突、候选任务回收、同 revision 重新准备后成功，以及失败前后真实 UDP 查询；用例目录为 `_fluxdns/p1-service-stage/` 的独立随机目录。闸门测试覆盖放行与丢弃，coordinator 测试覆盖不发布准备态和最终 CAS 竞争。`service::tests::` 筛选 61 项通过、3 项按原有声明 ignored（包含 cache/storage 同名测试；不执行手动性能与 1024-session 专项）；`runtime::` 筛选 65 项通过，含既有资源交错、mutation deadline、owner 回收回归。全部测试目标 `--no-run` 编译、fmt、文档和 diff 检查通过。

此处保证的是已有 DNS transport/resource 任务注册失败的前置拒绝，不声称日志/详情/新存储 owner 已加入事务或存在通用补偿器。operation/active_source 的正式应用回报和 v2 启动仍未接线；BC-03、BC-29 与 P1 退出条件继续保留。

### P1 请求 drain 子项（2026-09-07）

`ActiveRuntime::begin_drain` 同时唤醒入口和空闲连接，但不取消已经取得 request guard 的 dispatch。UDP 停止下一次 receive，TCP/DoH 停止 accept，并在当前响应完成后关闭旧连接；新 transport 同时使用新 Runtime 服务。进程 shutdown 仍通过 Supervisor 的 scoped/global cancellation 中止任务，不把热更新 drain 扩大为无损停机承诺。

三类 dispatch 均以请求入站时捕获的 deadline 为兜底，包括 Core 不合作的情况；不会从热更新时重新计算预算。超时取消原请求和 response handle，不伪造成功响应。guard 并发接纳失败也通过统一释放路径通知归零。服务任务完成后清除已 drain 的历史 Runtime owner 引用，避免已移除端口被历史实例长期持有。

Windows Rust 1.98.0 验证：新增 2 项真实 loopback 测试覆盖复用/差量重绑时 4 条旧 UDP/TCP/DoH POST/GET 请求仍返回旧 Core 响应、新请求已由新 Core 响应、旧任务回收和旧 UDP 端口可重新绑定；不合作 Core 的 200ms 原始预算能释放 3 类请求且新实例继续服务。`service::tests::` 筛选 63 项通过、3 项按原声明 ignored。这里的“已接纳”以 request guard 为边界，不声称操作系统尚未交给 Runtime 的流量、任意规模持续负载或跨平台已无丢包验收。

### P1 服务控制队列子项（2026-09-07）

[`service/control.rs`](../../../backend/src/service/control.rs) 提供单候选排队的 `ServiceControl`，另有一个由现有 `DnsService` 执行的应用槽。`try_apply` 只接受已准备且为 expected + 1 的 Runtime，队列满、已过期或 owner 已关闭时明确拒绝；服务循环消费时再次核对 revision/deadline，再走同一个 `reload_prepared`，不复制 Supervisor，也不增加重启/停止命令。

回执超时或通道断开是 `OutcomeUnknown`，不会自动重发；调用者丢弃回执也不撤销已接纳命令。关闭 owner 会停止接纳并拒绝尚在排队的命令；应用等待 mutation gate 时仍可响应退出信号，中断的命令回执只能报告未知。生产 ConfigMutationOwner 现在等待 service 最终回执并更新 ConfigStore 的可查询 operation；单个 HTTP 请求只负责受理，不持有该生命周期。

既有 6 项测试覆盖容量、入队/出队过期、关闭、真实服务循环换代、丢弃回执后继续应用、旧 revision 拒绝、真实绑定失败后旧 DNS 可查询、回执丢失/超时，以及提交锁等待期间退出。2026-09-08 的 v2 真实进程进一步通过组合 apply 将日志配置应用、持久化并重启复读，前后 UDP 查询持续成功；文件、SQLite 与 HTTP 证据见[配置事务生产接线](../configuration.md#p1-配置事务生产接线2026-09-08)。

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
| 配置切换 | `ConfigMutationOwner`、`ServiceControl`、`reload_prepared` | P1 组合 apply，复用进程服务；watcher 只观测 | reload/rebind/failure、真实 v2 Bearer HTTP/UDP/日志/文件联合验证 | P3 单模块写入未接线；不宣称所有平台组合已验收 |
| 有界停机 | `shutdown`、finalizer owner、stats-first | 正常信号及 fatal task 路径 | SQLite trigger 验证 stats 提交先于 300 条多批详情排空 | 已执行 SQL 无强制抢占保证；Unix 双信号 smoke 未执行 |
| 安全 panic hook | 固定分类、受限源码位置、backtrace 状态 | 异步 `main` 第一项安装 `std::panic::set_hook` | 独立子进程验证主线程/worker panic 不泄漏 payload、线程名或完整栈 | 不改变内部 task owner 的失败升级策略；安装前异常不覆盖 |

2026-09-05 的完整 Cargo 测试与 loopback 验证见[后台服务](background-services.md#本次验证)；没有真实 Unix 信号或外部部署 smoke 证据。

## 契约验证补充

以下是 `f65fb3f8bd68e1a40ca041d9a380859b44a3da0c` 之后工作树的测试实现；运行结果与可重复入口统一见[契约验证运行入口](background-services.md#契约验证运行入口)。不覆盖真实 Unix 进程信号，也不把内部 worker 回收等同于 Supervisor 的失败升级。真实 PID 的首/第二信号、deadline 耗尽分类与退出后清理仍未验收；相关专项已按用户要求结束，保留[验证范围与收口](background-services.md#验证范围与收口)中的已知边界，不生成替代通过记录。

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

现有 serialized activation CAS、旧 scoped task 退出、Supervisor panic 归因以及进程 panic hook 测试由全量回归复用。上述 owner 用例区分 adapter panic 与 worker 返回后的 join panic，未引入统一自动恢复策略；真实 Unix 首/双信号及其在途组合保留为未验收边界，不再作为本专项的活动待办。
