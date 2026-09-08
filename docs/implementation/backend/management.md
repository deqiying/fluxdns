# Management 实现

> 文档状态：有效
>
> 适用范围：正式 Management listener、认证、配置写入、只读查询与内嵌资源接线
>
> 最后核对：2026-09-08（P1 配置事务与文件操作路由）
>
> 核对基线：`4a5a5a7b13896f3b4c1d86fe4469a3afae38ac10` 加本次 P1 工作树
>
> 时间存储补充核对：2026-09-05，`43671f1685edcaf271d8e62c184a7f72f5a2cefe` 加业务时间迁移工作树；不扩大其他管理功能审计范围

## 入口与生命周期

[`app::run_command`](../../../backend/src/app.rs) 在 DNS candidate 绑定、coordinator 和 DnsService 创建并挂接日志 owner 后调用 [`ManagementService::bind_with_config_store`](../../../backend/src/management/server.rs)。后者消费 v2 loader 原文建立的 active ConfigStore 和 `service.control()`，调用 feature-aware 资源检查并要求 origin，创建 AuthState、SessionStore、配置事务 owner、只读 SQLite adapter 与 query service，最后绑定独立 HTTP listener。

`DnsService::attach_management` 持有管理状态并注册受监督 task。不是 DoH listener 的附加路由；`webui.enable: false` 不创建此链。`ManagementRuntime::reconcile_users` 识别内部写入指纹，并只在实际认证内容改变时撤销 session；普通配置指纹变化或用户排序不撤销会话，`shutdown` 仍撤销会话。

P1 会话回归（2026-09-07）：`AuthState::replace` 按名称规范排序后比较用户名和 password hash，返回是否变化，不输出 hash。既有 setup/router 用例扩充覆盖内部首次写入、普通配置指纹变化、用户增加、顺序变化和密码 hash 改变，`management::` 18 项通过；测试文件转入 `_fluxdns/p1-management-auth/` 的独立用例目录并核对清理边界。此处是显式应用后的认证回报边界；正式 app watcher 已改为[仅提示观测](lifecycle.md#p1-仅提示文件观测2026-09-07)，不会调用 `reconcile_users`。尚未提供 v2 配置状态端点或浏览器全局提示。

## P0 v2 契约

目标字段权威为 [v2 OpenAPI](../../../frontend/openapi/management-api-v2.yaml)，Rust 类型及有界解码位于 [`management::contract`](../../../backend/src/management/contract.rs)。除既有指标、配置只读、保留和历史端点外，P1 已注册组合配置校验/应用、operation 查询、外部差异、文件还原和持久化重试。它们均受 Bearer 保护，写请求额外要求 Origin/Fetch Metadata。P3 的 `/config/modules/{module}/validate|apply` 与 WS 仍未注册。

P1 BC-02 补充：严格变更类型已移到 [`config::edit`](../../../backend/src/config/edit.rs)，本模块重用而不另建协议形状。完整候选校验、活动源编辑、双文件观测、验证票据和操作记录已有 ConfigStore 内部入口，事实与验证边界见[配置参考](../configuration.md#p1-活动源与候选内部底座2026-09-07)。下表的 P0 decoder 不因此成为已接线的写入服务；BC-12 只开放读取，异步应用、外部差异与普通配置写入仍未注册。

| 契约 | 已落实的内部能力 | 正式接线与剩余边界 |
| --- | --- | --- |
| 模块配置 | 重用配置 DTO，严格 tagged union；创建/更新，无删除；客户端更新不含 `client_id`；系统只读投影不含 users/hash | P1 组合入口消费 `decode_candidate` / `decode_apply`；单模块 decoder 保留给 P3，但对应路由未注册 |
| 活动配置与文件 | active/runtime/persisted revision 分离；组合文件观测 token；源表达、生效值/来源、引用和独立 runtime 投影；外部差异、还原、同步重试请求 | 状态、差异、还原和重试已接线；DTO 不接受路径、整份 YAML 或 Secret 实际值 |
| 操作与失败 | preparing、applying、persisting、applied_synced、applied_unpersisted、rejected、compensation_failed、unknown；幂等 ID、校验 token、明确确认清单 | ConfigMutationOwner 独立于 HTTP 请求推进 Runtime 与持久化；运行成功不等于文件同步，unknown 不能自动重放 |
| 历史与实时 | 原始 ID/IP、当时匹配、当前名称、稳定记录 ID、历史 cursor 与提交 cursor 分离；指标不可用状态、WS 判别消息 | BC-09 已实现分片 storage 读口、cursor 上下文/水位完整性和 commit 后通知；BC-13 已接入当前名称投影与 Bearer HTTP，WS 鉴权/队列/replay 待 BC-24/25 |

大计数/序列使用十进制 `u64` 字符串，Rust 和 schema 均拒绝溢出与前导零；安全整数时间采用 UTC ms，耗时采用 us。请求中的安全时间上界由 REST/WS 共用验证器执行；输出时间仍为 Rust `u64`，生产投影接线时必须保持 schema 的安全整数边界。源 DTO 的相对路径、SecretRef 来源和缺失继承保留，duration 序列化为精确 ns 字符串；这不是可回写的完整 YAML 语法树，不能据此丢弃原注释或显式源表达。

所有预算在 OpenAPI `x-limits` 和字段 schema 中维护。已执行入站保护包括配置 4 MiB、变更 2 MiB/128 项、cursor 2048 bytes、历史查询 16 KiB/100 行/3650 天、WS 入站帧 128 KiB；BC-23 的在线身份表上限为 4096，超限窗口显式返回 `observation_gap`。文件读取以及 WS 连接/队列/replay 限额仍仅为契约，不能据此宣称运行时已受保护。

验证使用 [Rust 契约测试](../../../backend/src/management/contract/tests.rs)、[共享夹具](../../../backend/tests/fixtures/management-v2.json) 和 [Node schema 测试](../../../frontend/tests/contract-v2.node.mjs)：覆盖多态源值往返、只读注入、ID 修改、null、预算、精度、状态互斥、P1 路由清单及 P3 模块写路由缺席。真实 P1 HTTP/Runtime 证据见下述配置事务接线；WS 仍未实现。

## P1 配置状态内部投影（2026-09-07）

[`config_query.rs`](../../../backend/src/management/config_query.rs) 将真实 ConfigStore 快照映射到既有 `ConfigState` 和 `OperationResult`，不读取外部文件、不从 Runtime 反推配置、不返回活动原文、路径、身份/hash 或底层错误。`runtime_revision` 以十进制字符串表示，测试覆盖 `u64::MAX`；其余 opaque token 由与入站反序列化共用的有界构造器检查，schema 和生成类型无 wire 变化。

操作查询按原调用者返回冻结的版本与安全错误码，不用当前配置状态拼接旧操作结果。不同调用者、未知和过期返回 `Unknown`，不授权自动重放。配置同步状态与外部文件变化独立，精确自写识别及保留期见[配置状态事实](../configuration.md#p1-配置状态与冻结操作结果2026-09-07)。状态锁忙时返回 `OPERATION_BUSY`，不会阻塞 executor 等待同步文件事务。

[`config_query/tests.rs`](../../../backend/src/management/config_query/tests.rs) 使用真实临时双文件与 Windows 文件占用错误，覆盖结果冻结、调用者隔离、全部操作状态、五类文件状态和 u64 字符串边界；输出样本限定 `_fluxdns/p1-config-query-projections/`，不写入 Git。操作应用成功仍由测试模拟，正式写 owner、代理/client 与 HTTP 响应中断继续留待后续检查点。

## P2 模块配置只读 API（2026-09-08）

`GET /api/v2/config/state`、`GET /api/v2/config/system`、`GET /api/v2/config/modules/{module}` 和 `GET /api/v2/retention` 已进入正式 query router。业务接口只接受 Bearer；v2 失败响应使用带空 `field_errors` 的固定 `ErrorEnvelope`，未知模块返回 `NOT_FOUND`。同路径 POST、预校验、应用、外部差异和 retention preview 未注册，不借只读接线开放 P3 写能力。

模块读取在一次 ConfigStore 锁内冻结活动源、文件观测和 revision，再要求 `active.runtime_revision` 与当前 Runtime 相同；不一致返回 `SERVICE_UNAVAILABLE`。`values` 只包含 URL 指定的一个模块，保留源 DTO 的相对路径、duration 和 SecretRef 来源；`effective` 从同代 ResolvedConfig 投影 global/strategy/client/explicit 来源；`references` 只列该模块的出站引用。listener bind、Hosts/RuleSet 资源新鲜度及 DNS 快照状态来自同代 Runtime，不通过重读文件或反序列化响应重建运行态。

系统端点仅返回源配置中的 `work.path`、`rules_path`、统计库/分片路径与 WebUI enable/address/port/public origin，不返回 users、password hash、配置摘要、实际 Secret 或进程解析后的任意路径。保留状态复用生产 `RetentionCoordinator`，采样受管详情主文件和 WAL；`pending_reclaim_bytes` 仅累计 pending/failed manifest 日期，下一调度时间按服务器当前时区严格取下一次本地 01:00。

Windows 使用本批 debug binary 与独立 `_fluxdns/bc12-http/` v2 配置完成真实进程验证：一次性 setup 返回 201 和 Bearer，配置状态、系统白名单、十个模块及 retention 共 13 个请求均返回 200；每个模块响应只含同名 `values`。仅携带刷新 Cookie 的配置请求返回 401。测试进程已停止；这不覆盖浏览器、HTTPS 反向代理、Linux、写接口或 WS。

本批 Windows 验证：`config::` 106 项、`management::` 22 项、`cargo check`、全部测试目标 `--all-targets --no-run` 和 fmt 通过；12 个配置状态及 10 个真实操作投影经现有 AJV 对 v2 schema 校验通过，覆盖 8 种操作状态、5 种文件状态和 5 种同步状态。前端 typecheck、schema 3 项、Vitest 7 文件 38 项通过；Node/Vite 的沙盒 `spawn EPERM` 经批准重跑解决。未运行完整 Cargo suite、v2 生产启动/HTTP、浏览器、跨平台或性能验收。

## P2 新版解析历史查询（2026-09-08）

`POST /api/v2/queries/search` 与 `GET /api/v2/queries/{record_id}` 已进入正式 Bearer query router，直接消费 [`DetailShardStore`](../../../backend/src/storage/detail_query.rs) 的跨日读口，不让 handler 持有 SQLx pool。列表先以一次 active ConfigStore 快照把 `client_name` 模糊匹配解析为完整当前 `client_id` 集合，再由 storage 在分页前应用全部过滤；空 ID 集合也显式返回空结果。原始 ID/IP、历史匹配来源和匹配 ID 保持写入时事实，当前名称及 `directory_revision` 只作为同次读取投影，不补造或重匹配历史。

qname 在进入 storage 前归一化为小写绝对名，qtype 接受标准名或 `TYPE<n>`；详情耗时从存储毫秒精确投影为微秒。缓存命中的顶层 upstream 字段保持空值，写入时 producer 的 strategy/目标/实际 upstream 单独放入 `cache_producer`。记录 ID、前后 cursor、commit cursor 和 retention revision 沿用 BC-09 的 opaque/十进制契约；cursor 篡改或水位失效返回 410 `CURSOR_EXPIRED`，非法 filter/record ID 返回 400，已过期或不存在记录返回 404，storage/目录代次异常返回 503。

Windows 定向测试用两个真实 UTC 日 SQLite 分片覆盖跨日稳定分页、分页前名称过滤、无尾点 qname、缓存 provenance、详情定位、非法 qtype/cursor 与 Bearer 拒绝。生产 debug binary 又在独立 `_fluxdns/bc13-http/` 目录完成真实 login、`doggo` UDP 请求、详情分片提交及 Bearer HTTP：列表和详情均为 200，返回同一稳定 ID；无 Bearer 列表为 401。实际两类响应另经当前 OpenAPI AJV 校验。最终完整 Cargo 为 836 passed、3 ignored，全部测试目标编译、build 和 fmt 通过；前端 typecheck、20 文件 84 项 Vitest、build 与 4 项 v2 schema contract 通过。Clippy `-D warnings` 仍被本次改动前已有的 6 项 lint 阻断，本项没有新增 lint；一次此前成功运行后的默认并发 Cargo 重跑发生无具体失败用例的 Windows `STATUS_STACK_BUFFER_OVERRUN`，立即同命令重跑通过。测试进程已停止；这不覆盖浏览器、WS/replay、HTTPS 反向代理、Linux、约 10 客户端或 core 2ms 性能。

## P1 外部配置差异内部投影（2026-09-08）

[`config_query/external.rs`](../../../backend/src/management/config_query/external.rs) 消费 ConfigStore 的[固定源输入](../configuration.md#p1-外部差异输入内部能力2026-09-08)，比较全部十个可写模块的类型化源值。按同一命名空间的 `name` 配对，不猜测改名、不授权删除；客户端 ID 的读取差异不改变普通编辑白名单。缺失继承、SecretRef 引用、路径及资源内部顺序保留，类型化等价表达不制造假差异。

`work/database/webui` 只报告类别，users/hash 变化只报告 `protected_credentials`；不返回原始解析错误、整份 YAML、Secret 实际值或管理认证 token。普通资源 URL query 按既有源 DTO 保留，不因含有 query 而整体隐藏，也不构造可误保存的替代 URL。凭据传输方式与资源 URL 是不同契约。

完整输出最多 128 项、序列化 JSON 最多 2 MiB；超限整体报错，不截断。写入计数器按真实 UTF-8 和 JSON escaping 计费，另外检查 schema 的字段长度与安全整数。输入完整配置无效时只返回双版本绑定及安全 `parse_error`，不部分采用。

Windows 测试使用真实受管文件，覆盖十模块、嵌套 DoH/TLS/组/内联类型、引用失败、只读和 hash 隔离、128/129 项、输出字节与字段超限；15 个实际投影经现有 AJV 对 v2 schema 验证。正式路径为 `GET /api/v2/config/files/diff`，由阻塞 owner 执行同一投影；前端组合采用仍等待各 P3 业务表单。

## P1 配置事务与文件操作（2026-09-08）

[`config_mutation.rs`](../../../backend/src/management/config_mutation.rs) 注册 `POST /api/v2/config/validate`、`POST /api/v2/config/apply`、`GET /api/v2/config/operations/{operation_id}`、`GET /api/v2/config/files/diff`、`POST /api/v2/config/files/restore` 和 `POST /api/v2/config/files/retry`。没有注册 P3 单模块 validate/apply、任意 YAML、PATCH、DELETE、restart 或 stop。写请求复用 v2 Bearer，并返回 v2 `FORBIDDEN` 形状的同源拒绝；候选路由单独允许 2 MiB body，其余既有路由继续使用 16 KiB 默认上限。

组合 apply 在阻塞线程完成 ConfigStore 受理后立即返回 202；后台 owner 完成 Runtime prepare、ServiceControl 回执和持久化，客户端按相同 operation ID 查询。文件还原/重试在阻塞 owner 中执行，HTTP 取消不终止已经开始的写盘；已记录失败优先返回冻结 OperationResult，受理前冲突返回 ErrorEnvelope。operation 按用户名隔离，不在响应或日志中返回源正文、路径身份、底层错误或认证凭据。

Windows 真实 HTTP 使用一次性 setup/后续 login 获取内存 Bearer，完成校验、apply、operation 轮询、状态、差异、还原和重试。日志父目录缺失返回冻结 `APPLY_FAILED` 且 Runtime/文件未变；有效日志候选得到 `applied_synced`、Runtime revision 2、双文件一致。仅 Cookie 的现有配置读拒绝证据沿用 BC-12；本批未验证 HTTPS 反向代理、Linux、磁盘满、响应恰在 service commit 后丢失或 P3 模块写入口。

## 路由与保护

[`router.rs`](../../../backend/src/management/router.rs) 的 `build_router` 组装 setup/login/refresh/logout、受 Bearer 保护的 session、[`query.rs`](../../../backend/src/management/query.rs) 查询路由和 P1 配置事务路由；未知 API 与 SPA fallback 隔离。既有页面字段/状态码以 [v1 OpenAPI](../../../frontend/openapi/management-api-v1.yaml) 为准，v2 端点以 [v2 OpenAPI](../../../frontend/openapi/management-api-v2.yaml) 为准。

router 固定保护包括普通 JSON body 16 KiB、P1 配置候选 2 MiB、URI 4 KiB、64 个 header/16 KiB header bytes、256 个并发请求和 15 秒总请求 timeout。另有 setup/login 限流、写请求 Origin/Fetch Metadata、request ID 和统一错误处理。这些是实现常量，不是额外 YAML 字段。

[`auth.rs`](../../../backend/src/management/auth.rs) 的 `validate_setup_credentials` 与 `hash_password` 使用 12 至 1024 bytes 密码、Argon2id 19 MiB/2 iterations/parallelism 1；登录兼容 bcrypt。密码不 trim，用户名使用配置层共享规范。

[`session.rs`](../../../backend/src/management/session.rs) 使用有界内存会话，访问/刷新凭据的职责和期限见下节；HTTP/HTTPS 两种 Cookie 名称与 Secure 策略见 [Management 设计](../../architecture/management.md)。没有独立持久化 session 数据库。

## P1 Bearer 业务鉴权（2026-09-08）

按用户追加决定，正式 `/api/v1` 的受保护 session 与全部业务查询均只读取 Authorization Bearer，不接受 Cookie、URL query、重复或非法 Authorization 作为后备。初始化/登录返回 `AuthSession`，包含无凭据 session、短期 access token、Bearer 类型与安全整数 UTC ms 过期时间；同时设置独立的 HttpOnly 刷新 Cookie。`POST /auth/refresh` 只接受该 Cookie 并校验 Origin/Fetch Metadata。`/auth/logout` 用有效 Bearer 撤销关联会话并清除 Cookie，无有效凭据时仍幂等，不用 Cookie/query 选择会话。API 响应 no-store，401 带 Bearer challenge。

SessionStore 复用原 24 小时绝对/30 分钟空闲期限、4096 全局/16 单用户容量。访问凭据 5 分钟有效，剩余 30 秒内换发；并发刷新复用当前值，旧值只活到原期限，每会话最多两项访问索引。到达会话绝对期限不反复生成新凭据；过期、容量淘汰、认证更新、登出和进程关闭同时回收相关索引。两类随机凭据不互换，刷新凭据不进入响应正文。没有新增依赖或持久会话存储。

[`router/tests/bearer.rs`](../../../backend/src/management/router/tests/bearer.rs)、SessionStore 和查询路由测试覆盖两类凭据隔离、重复/非法 header、Origin、期限、索引回收及既有查询；[前端接线](../frontend/application.md#p1-bearer-接线2026-09-08)统一在业务请求中附加 Bearer。当前与目标 v2 OpenAPI/生成类型均已同步；BC-23 后仅两个指标端点已注册，不能由认证或指标验证推断新版配置 owner 已启动。

Windows 使用 Bearer 子项当时的内嵌 binary、独立 `_fluxdns/p1-bearer-ui-20260908-0055/` 配置/SQLite 和 loopback 端口完成真实 HTTP 12 项检查：登录、Bearer session/overview、Cookie-only/凭据互换/非法 Bearer 拒绝、跨源刷新拒绝、刷新、当时尚未注册的 v2 返回 404、登出及两类凭据失效。浏览器 Network 已确认业务请求有 Authorization 且无 Cookie，刷新请求相反；还验证初始化、页面刷新恢复、重新登录和 Cookie 清除。该历史检查不覆盖随后新增的两个指标端点；HTTP 直连也不证明外部 HTTPS 代理，浏览器 WS 凭据传递留 BC-24 核定。

本批最终回归：Management 39 项、Config 110 项、Cargo check、全部目标 no-run、fmt、前端 typecheck/schema 4 项/Vitest 9 文件 47 项/build 通过，当前及目标类型连续两次生成一致，文档检查与 diff 检查通过。Node/Vite 测试按已核实的沙盒 spawn 限制获准在沙盒外运行；未运行完整 Cargo suite、跨平台或性能验收，所有临时服务与运行数据均限定本次 `_fluxdns` 测试目录。

## P1 服务与进程指标（2026-09-08）

[`metrics.rs`](../../../backend/src/management/metrics.rs) 提供进程级唯一 `MetricsOwner`。UDP、TCP 和 DoH GET/POST 在入站基础校验完成且 `Runtime` 成功接纳后调用同一计数入口；上游重试、single-flight 分支和被容量拒绝的请求不会重复或提前计数。请求窗口使用 601 个有界秒桶保留当前不完整秒：QPS 为最近 60 秒请求数除以 60，RPM 为最近 600 秒请求数除以 10；趋势仅输出已完成的最多 600 个秒桶和 10 个分钟桶。

在线身份只保留 SHA-256 脱敏 key 与最后出现秒。有效 `client_id` 优先，否则使用规范化后的有效 IP；ID、IPv4 和 IPv6 使用不同域前缀，IPv4-mapped IPv6 归一为 IPv4。表容量固定 4096，容量丢失影响的 60 秒窗口返回 `observation_gap`；进程启动未覆盖 60/600 秒时返回 `warmup` 和 `observed_seconds`，没有请求只有在完整窗口后才报告真实 0。该路径不依赖详情开关，也不把原始身份写入统计库或 telemetry label。

生产 `DnsService` 只启动一个受 Supervisor 管理的 `management.metrics` 任务，每秒更新共享进程快照。Windows 使用既有 `windows-sys` 的 ProcessStatus、Threading 和 ToolHelp API 读取 RSS、进程 CPU 时间与线程数；Linux 条件编译实现读取 `/proc/self/status`、`/proc/self/stat` 和 `/proc/stat`，本批未在 Linux 实测。首次 CPU 样本返回 `warmup`，直接读取失败返回 `sampling_failed`，非 Windows/Linux 返回 `unsupported`，超过 3 秒未刷新返回 `observation_gap`；DNS 请求不等待 OS 采样。

`GET /api/v2/service/metrics` 与 `GET /api/v2/system/runtime` 复用同一 `MetricsOwner` 和 RSS 快照并要求 Bearer。受控时钟测试覆盖精确窗口、趋势、NAT、同 IP 不同 ID、unknown、mapped IPv4、容量恢复和采样缺口；跨 transport 真实 socket 测试核对 16 个接纳请求只计 16 次，Windows 测试读取真实 RSS/CPU/thread。

Windows 使用当时 debug binary、`_fluxdns/p1-metrics-http-20260908/` 独立配置和 loopback 端口 `18123`/`15399` 完成生产链启动：配置 validate 通过，Bearer 请求两个指标端点均为 200，QPS/RPM/在线身份在未满窗口时返回 `warmup` 与覆盖秒数；两处 RSS 值相同且大于 0，CPU/thread 可用且非负。该历史检查发生在 BC-12 前，不能作为新增配置端点证据；Linux、浏览器、性能和 BC-24 WebSocket 本批未验证。

## 首次用户写入

`post_setup` 完成凭据检查与 hash 后，通过 [`ConfigStore::create_initial_user`](../../../backend/src/config/store.rs) 写入，成功提交后才发布用户快照与 session。实际链为：

```text
try_lock -> ConfigFileLock -> reread source / fingerprint check
 -> source_edit::create_initial_webui_user
 -> ConfigLoader::load_candidate_bytes without snapshot
 -> commit_candidate: stages / journal / two target replacements
 -> publish expected + self-written fingerprint -> update auth / session
```

[`source_edit.rs`](../../../backend/src/config/source_edit.rs) 使用 source-preserving YAML 编辑，仅修改 users；不支持的语法明确失败。writer 输入上限为 4 MiB，普通 loader 上限为 8 MiB，因此“可加载”不等于“可首次初始化写回”。源/快照冲突和恢复约束见 [Config 设计](../../architecture/backend/modules/config.md)。

`recover_pending_transaction` 仅接在正式 run 的加载前；`validate` 不执行事务恢复。两文件替换有 journal，但不是整体原子 rename；跨平台权限、真实中断和各 crash point 仍需验收，不能仅凭函数存在宣称完整恢复矩阵通过。

## 查询数据流

| API 主题 | 实际数据来源 | 限制 |
| --- | --- | --- |
| overview | coordinator summary、resolution metrics、只读数据库计数 | 详情关闭或无数据时按响应原因区分不可用，不把缺数当零 |
| runtime | 当前 RuntimeSnapshot、listener 与摘要 | 不是直接操作 listener 的命令入口 |
| health | telemetry health snapshot | 缺少观测来源时不能推断健康 |
| statistics | `ManagementStorageRead` 的聚合查询 | 时间范围与维度校验 |
| queries | `ManagementStorageRead` 的详情查询 | 有界分页/过滤/排序，历史脱敏行返回 `legacy_redacted` |
| resources | runtime 资源 snapshot 元数据 | 只读，不触发刷新 |
| system | 版本、进程/构建与功能元数据 | 不提供配置秘密或绝对路径 |
| service metrics（v2） | `MetricsOwner` 的请求窗口、在线身份和共享 RSS 快照 | 暖机、容量截断及采样缺口显式不可用；不返回原始身份 |
| system runtime（v2） | `MetricsOwner` 的共享 OS 采样快照 | 仅 Windows 实测；Linux 条件编译实现未实测 |
| config state/system/modules（v2） | active ConfigStore 与同 revision RuntimeSnapshot | 系统字段白名单；模块集合最多 1024，revision 冲突返回 503 |
| retention（v2） | 生产 RetentionCoordinator、stats layout、详情分片与 manifest | 主文件加 WAL；下一次为服务器时区本地 01:00；只读不触发回收 |
| queries（v2） | active ConfigStore 目录快照与 DetailShardStore 跨日读口 | 过滤先于 keyset 分页；原始/历史事实不重匹配；cursor 绑定过滤、水位和进程 epoch |

[`ports/management.rs`](../../../backend/src/ports/management.rs) 定义领域读口；[`SqliteManagementReadModel`](../../../backend/src/storage/management_read.rs) 使用独立只读 pool、绑定参数和固定 SQL。query service 固定 5 秒 deadline、默认 20/最大 100 行分页和最长 31 天统计窗口。

业务 schema v6 直接读取 `event_time_utc_millis` 为 `i64`，overview 范围比较和查询时间排序不再逐行 `CAST`；同毫秒仍以 ID 确定顺序，升降序保持对称。读口的 `occurred_at_millis` 仍为 UTC Unix 毫秒，query service 继续输出原日期格式，不更改 OpenAPI/前端类型。迁移及字段单位见[业务时间存储](background-services.md#业务时间存储)。

查询详情可供所有已认证用户读取 qname、有效 client IP、配置标识、upstream provenance 与有界 answer；DNS wire、request digest、route 文本和 SecretRef 不进入 API。core duration 的历史缺失值不会补造。

## 静态资源与证据

[`assets.rs`](../../../backend/src/management/assets.rs) 在 `webui-embed` 下通过 `RustEmbed` 派生的 `WebAssets` 读取 `frontend/dist`，排除 map，处理 MIME、ETag、HEAD、条件请求和 fallback；打包脚本另行检查 dist/index.html，仓库没有自定义 `backend/build.rs`。

启用 embed feature 时，`ensure_available` 检查内嵌 index。**未启用该 feature 时检查返回 Ok，Management API 仍可启动，但没有 SPA 资源**：接受 HTML 的前端 fallback 返回 503，普通资源缺失返回 404。不能写成默认 binary 必须有 SPA 才能启动管理端。打包行为与历史 Windows 证据见[交付实现](../delivery.md)。

| 能力 | 代码实现 | 正式入口接线 | 验证证据 | 已知限制 |
| --- | --- | --- | --- | --- |
| setup/auth/session | router、AuthState、SessionStore | ManagementService -> DnsService | P1 Bearer 定向测试、真实 HTTP 和浏览器 Network/Storage，见上节 | 未验证外部 HTTPS 代理及 v2/WS |
| users 事务 | source_edit、ConfigStore、journal recovery | setup 写入，run 启动恢复，watcher 对账 | 本轮核对；存在双路径恢复与 Busy 竞争测试 | 完整跨平台 crash/权限矩阵待验收 |
| 七个 v1、两个指标、配置/保留与历史 v2 只读 API | ManagementQueryService + StorageRead port + DetailShardStore + MetricsOwner + RetentionCoordinator | app 注入 active ConfigStore、真实 coordinator/DB/telemetry/metrics/retention/detail store | BC-12/13 路由使用真实临时 SQLite；生产 Bearer HTTP 证据见本节 | v2 认证、写入与 WS 未注册；未执行浏览器 smoke |
| 内嵌 SPA | assets + build feature | bind 前 ensure_available | 静态；历史证据单独标注于交付文档 | Actions/Linux/macOS 发布未由静态代码证明 |

本页 2026-09-05 原核对仅有静态证据；后续 P1 的运行证据分别列在对应小节。已执行的 Bearer HTTP/浏览器验证不等于反向代理或完整配置事务故障矩阵均已验证。

本次时间改型补充了真实 SQLite 定向用例：跨位数升降序、同时间 ID 顺序、overview 包含边界和查询计划中的时间索引。完整 Cargo 结果见[后台服务验证](background-services.md#本次验证)，不等价浏览器 smoke。
