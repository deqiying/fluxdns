# 后端契约差距核对计划

> 文档状态：草案
>
> 计划状态：待评审
>
> 适用范围：12 个后端模块按当前源码核对后，仍需决策的契约差距与非 v2 运行验收
>
> 代码基线：`8223d819efb83fed642900e6b121825083e8c1dd`
>
> 最后核对：2026-09-05（源码、schema、正式装配及测试定义；未执行产品测试）

## 问题与依据

已直接更新[模块设计](../architecture/backend/modules/README.md)及直接受影响的 implementation/configuration 文档，以当前代码为事实基准。以下不再登记为“尚待实现”：正式 Policy/Cache/Upstream 链路、现有资源刷新、SQLite migration、受管 late finalizer 和解析完成事件，这些已经有生产接线。

文档中的旧类型/结构草图也已校正：Config 实际输出、CIDR/域名索引、RuntimeSnapshot 对象图、分层 task owner、确定性退避等，以现有源码描述，不因缺少原设想的类名或 trie 自动新增开发任务。

本计划只保留行为承诺仍未落实、原语未接线或需要运行证据的事项。更新文档不代表这些偏差已经获得产品决策，也不授权修改代码。

## 已确认的契约差距

| 项目 | 分类 | 当前源码事实与原要求差异 | 后续最小决策 |
| --- | --- | --- | --- |
| 安全 panic 输出 | 明确未实现 | [main.rs](../../backend/src/main.rs) 使用默认 panic hook；源树无 `set_hook`。旧 Application 要求的 payload 脱敏没有专门实现 | 确认允许的 panic 字段与退出契约，实施安全 hook 或明确修订要求；不能宣称默认输出已安全 |
| 缓存物理容量 | 明确未实现 | [sqlite.rs](../../backend/src/cache/sqlite.rs) 将预算交给 [prepare_snapshot](../../backend/src/cache/persistence.rs)，只限制编码快照；没有 `max_page_count`、按实际主库大小 checkpoint/收缩的路径 | 先确定编码预算还是物理容量契约，评估页/sidecar/重写空间后再实施；当前不能据此承诺磁盘硬限额 |
| 缓存访问热度及写放大 | 明确未实现 | `prepare_snapshot` 按 inserted_at/version/key 淘汰；SQLite 每批全量解码、合并、事务重写。没有 last-access bucket、近似 LRU 或 SQL key/namespace 索引 | 评审原近似 LRU 与增量持久化要求是否仍需要，量化写放大后决定；现有行为不改称 LRU |
| bootstrap TTL 缓存 | 原语存在、生产未接线 | [bootstrap.rs](../../backend/src/upstream/bootstrap.rs) 有 AddressResolutionState/AddressCachePolicy；[TokioDohAddressResolver](../../backend/src/upstream/http.rs) 每次查询 A/AAAA 后仅返回地址，没有使用该状态 | 确定 TTL/失败复用/地址池失效边界，再决定接入；client pool 复用不替代地址缓存 |
| 资源条件请求 | 明确未实现 | [fetcher.rs](../../backend/src/resource/fetcher.rs) 无条件 GET、禁重定向、只收 2xx，modified_at=None；[fetch port](../../backend/src/ports/effects.rs) 无 validator 字段。旧 ETag/304 描述未落实 | 确认是否保留条件请求要求，再设计 validator、304 和快照刷新语义 |
| group 执行语义 | 需确认行为契约 | [executor.rs](../../backend/src/upstream/executor.rs) 有 late sink 时 Positive 也移交剩余任务 drain；无 sink 的非 Positive 可能等待全组。load-balance 只占用 primary lease，重试按配置顺序，非逐 attempt least-in-flight | 明确首响应/剩余任务取消、sink 可选路径及 primary 计数口径；不得直接宣称满足旧的完整语义 |
| 进行中请求的停机响应 | 需确认行为契约 | [service.rs](../../backend/src/service.rs) 的 UDP/TCP/DoH dispatch 与 cancellation 竞争，停止时可以直接取消并关闭；5 秒仅为回收总预算，不保证已读完整请求最终写回 | 决定保留立即取消还是要求 drain 已读请求，补充三协议时序测试后再修改 |
| Storage 启动探针与预算 | 明确未实现 | [StorageRuntime::open](../../backend/src/storage/service.rs) 调用 [connect](../../backend/src/storage/sqlite.rs) 建库/升级，再核对版本；没有独立写入/回滚探针，connect 未整体包裹 open deadline | 确认写入探针和完整启动预算要求，覆盖已有库及 I/O 卡住场景 |
| Storage 停机优先级 | 需确认行为契约 | 生产 StorageRuntime 先等独立 detail task，再调用 StorageService flush stats；内联 facade 才是 stats → detail → backend，不满足笼统的“stats 始终优先” | 确认共享 deadline 下 stats/detail 优先级，验证慢 detail 是否耗尽 stats flush 时间 |
| 完整 span/attempt/聚合 metrics | 部分原语存在、生产未接线 | [observability.rs](../../backend/src/observability.rs) 正式使用 TelemetryWriter + event layer；ObservabilityRegistry/EventWriter 未在 app/service 构造，无完整 span 树。[resolution.rs](../../backend/src/resolution.rs) 的 attempt_outcome 是请求终态维度，非逐 attempt 流 | 先确认需要保留的观测能力与成本；不把指标枚举、测试 registry 或存在性字段写成全部已接线 |

## 现有原语的接入可行性

这部分是接入评估，不表示本轮已经实施。优先复用现有生产链路，不因原语存在就并列启用两套状态或 writer。

| 对象 | 能否接入 | 接入前必须解决的边界 |
| --- | --- | --- |
| AddressResolutionState / AddressCachePolicy | 可以复用，是明确的生产能力候选 | 状态以 upstream ConfigId 为 key，而 DohAddressRequest 只有 host/port/bootstrap；需在 registry/connector/resolver 边界对齐稳定身份。还需定义并发查填、取消与 deadline、容量、TTL 下限、配置换代清理及地址变化后的 HTTP pool 选择；不能持锁跨网络 await |
| ObservabilityRegistry / EventWriter | 计数/状态原语可复用，不适合整套直接挂到现有服务旁边 | 旧 Component/MetricName/TypedEvent 与 ports telemetry 模型不同，health 仅固定四类组件；EventWriter 默认 flush 使用 NoopSink。应先统一到现有 TelemetryWriter/HealthSink 的模型与 owner，避免双队列、双健康状态和重复计数；span/attempt 生产者仍需开发 |
| SecretProvider | 只有 trait，不能当作完整 adapter 接入 | 实际 env/file 读取已由 ResolvedSecretRef accessor 承担；如为可替换 I/O 引入 adapter，需复用现有校验、限额与脱敏，不重复实现配置读取。不是当前功能缺失的阻塞项 |
| MemoryCacheStore、文件 persistence、ConfiguredDnsCore、fake | 不应为了“接上代码”替换生产默认 | 它们承担替代实现、简化构造或测试职责；当前默认 PolicyDnsCore + Moka + SQLite 已接线。只有明确的产品需要才改变默认装配 |

资源 ETag/304、完整 tracing span/attempt 流、安全 panic hook 不属于“已有完整实现、只缺调用点”：需要补充契约或实现，再接入并验证。

## 已实现但未完成运行验收

下表“已有证据”仅指本轮读到的源码和测试定义，不表示本轮运行通过。

| 范围 | 已有实现 / 测试入口 | 尚缺的证据 |
| --- | --- | --- |
| late-window / owner | [dns/policy.rs](../../backend/src/dns/policy.rs) 有 latest-runtime refresh/late sink 测试；[service.rs](../../backend/src/service.rs) 有 `shutdown_closes_finalizers_from_previous_and_current_runtime` | 跨 revision、producer lease、首/晚响应、取消、sink 满和 shutdown 的组合矩阵；先区分上表行为差距 |
| 真实数据库故障 | [cache contract tests](../../backend/src/cache/backend_contract_tests.rs)、[storage contract tests](../../backend/src/storage/backend_contract_tests.rs)、SQLite Busy/DiskFull hooks | 隔离介质下的真实 busy/full/权限/恢复；旧数据、pending/ledger、gap、DNS 时延和资源占用，不能用注入 hook 替代 |
| Adapter conformance | [ports/testing.rs](../../backend/src/ports/testing.rs)、service 的跨 UDP/TCP/plain DoH 测试及各出站 adapter 用例 | direct/bootstrap/connect_ip/SOCKS5/SOCKS5H、Host/SNI、TLS/PROXY、deadline/cancellation 的实际组合；不存在统一运行时 conformance 门禁 |
| Runtime 并发与时间控制 | [coordinator.rs](../../backend/src/runtime/coordinator.rs)、[prepared.rs](../../backend/src/runtime/prepared.rs)、[supervisor.rs](../../backend/src/runtime/supervisor.rs) 有合并、CAS、rebind、retry/drain 测试 | Policy/metadata 分步发布、旧 task 迟到、资源与 reload 竞争、内部 owner panic；FakeClock 不代表全部 Tokio timer 可统一推进 |
| Storage migration 与压力 | [sqlite.rs](../../backend/src/storage/sqlite.rs) 有升级/重开/幂等用例，`ledger.rs`/`stats.rs` 有批次重试原语 | 各旧版本升级、跨午夜/late event、积压保护、软硬详情容量、故障恢复及停机优先级 |
| DoH/TLS 安全与限额 | [doh.rs](../../backend/src/transport/doh.rs)、[system_socket.rs](../../backend/src/runtime/system_socket.rs) 有 wire/header/GET/POST/TLS/PROXY 用例 | 真实 TLS/forwarded 信任链、坏连接隔离、1,024 session 上限、长期连接恢复；UDP 顺序执行/分配策略不等同通用 admission limiter |
| Unix 信号与长期负载 | [service.rs](../../backend/src/service.rs) 有 SIGTERM 分支和第二信号等待，源码保留本机 profile 测试 | Unix 真实进程双信号；冻结硬件、QPS、并发与资源预算后长期压力，不沿用旧百分比 |

## 目标与非目标

静态分类已形成，下一步只对保留的行为要求逐项决策并批准最小实施或验收范围。不新增 DoT/DoQ、主动健康检查，不为了对齐旧类型草图新增 SecretProvider adapter、TransportProfile 或 trie，不顺手重构 DNS 模块。

这是关键契约核对，不是逐函数正确性审计、完整性能评估或安全认证。用户要求以代码为准，已据此更正当前能力；对原来更强的安全、容量、取消和观测承诺，仍显式保留差距，不能通过改文档静默关闭。

## 步骤、风险与退出

1. 评审上表每项是保持当前行为、缩减原要求，还是批准最小代码修改；不把文档更新授权扩展为产品实施授权。
2. 为获批行为和现有验收缺口建立定向矩阵；优先使用项目现有工具/fake，真实环境不足时保留限制，不擅自安装或操作真实故障介质。
3. 按批准范围实施与定向验证，同批更新 implementation；契约确实改变才更新 architecture。
4. 所有登记项有实现/验收证据或明确的范围决策后收口；确认新逻辑已沉淀到对应 implementation、改变的设计已同步到 architecture，再删除本计划与索引项。

风险是把缺验收写成未实现，或把已有缺陷通过文档改成合法行为。本轮完成设计正文对齐与静态差距分类，没有执行产品修复、Cargo、故障注入或性能测试；[v2 专属验收](webui-v2-management-integration.md)独立管理。计划仍为待评审，不因本轮文档交付而关闭。
