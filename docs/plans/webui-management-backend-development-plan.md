# WebUI 管理后台后端重构开发计划

> 文档状态：有效
>
> 计划状态：实施中
>
> 适用范围：管理后台配套 Rust 后端、热配置、新存储基线、HTTP/WS 契约及 Windows 验收
>
> 代码基线：`21fd23f3f711e2f7acc712b9ff715915c5248180`（2026-09-07 根据确认决策核对配置、Runtime、日志及管理入口；未运行测试）
>
> 总览：[开发总计划](webui-management-development-plan.md)
>
> 设计依据：[配套后端重构方案](webui-management-backend-refactor.md) · [需求与图稿解释](webui-management-requirements.md)

已确认方向和剩余技术核定见[决策清单](webui-management-decisions.md)。BE-02 的跨端状态机统一由[配置热更新专项](webui-management-config-runtime-plan.md)维护，本计划负责后端实施拆分。

## 1. 范围与实现原则

本计划按确认决策重订 B1-B8，保留身份矩阵、快照协议和保留算法，取消旧版迁移与兼容。2026-09-08 已追加授权 P2 核心数据与阶段性本地提交；不 push。BC-12/13 出现正式生产接线阻塞后，用户明确授权把 P5 BC-26 提前到当前任务；BC-27、旧数据迁移和 P3 写接口仍不在授权范围。

- 沿 `Config -> Runtime -> DNS/Policy -> ports -> adapters` 扩展；Management handler 不直接操作 SQLx pool、DNS 缓存集合或源 YAML。
- 请求路径只捕获 typed 事实、执行 DNS 与发布有界事件；快照、历史查询、名称关联、清理和磁盘写入全部在后台。
- 复用现有候选校验、CAS、进程 owner、Supervisor、ConfigStore journal、SQLx 和 batch ledger；不能再建立后台专属的第二套配置或存储权威。
- active/persisted/file revision、运行 revision、目录 revision、缓存 owner/generation、存储 layout version 和 WS stream epoch 各司其职。
- 使用新 schema 和新数据基线，不维护旧身份/统计 legacy 读取；新生产接线完成后删除直接被替代旧代码。
- 常见组件/依赖按 D-08 已获任务范围授权，优先复用并说明必要性；不借本阶段升级工具链或替换框架。

## 2. 任务与源码边界

| 任务 | 内容 | 主要现有入口 | 依赖 |
| --- | --- | --- | --- |
| BE-01 | 版本、配置与 API 契约 | [model](../../backend/src/config/model.rs)、[resolve](../../backend/src/config/resolve.rs)、[validate](../../backend/src/config/validate.rs)、[migrate](../../backend/src/config/migrate.rs)、[OpenAPI](../../frontend/openapi/management-api-v1.yaml) | 总计划 D-01 至 D-07 |
| BE-02 | 活动配置、应用后持久化、外部差异与热日志 | [store](../../backend/src/config/store.rs)、[source_edit](../../backend/src/config/source_edit.rs)、[service](../../backend/src/service.rs)、[app](../../backend/src/app.rs)、[observability](../../backend/src/observability.rs) | BE-01、配置专项 |
| BE-03 | 客户端身份和匹配 | [DNS context](../../backend/src/dns/context.rs)、[client](../../backend/src/policy/client.rs)、[DNS Policy](../../backend/src/dns/policy.rs)、[observation](../../backend/src/ports/observation.rs)、[resolve_log](../../backend/src/storage/resolve_log.rs) | BE-01 |
| BE-04 | 独立缓存快照 | [cache service](../../backend/src/cache/service.rs)、[memory](../../backend/src/cache/memory.rs)、[moka](../../backend/src/cache/moka.rs)、[persistence](../../backend/src/cache/persistence.rs)、[cache runtime](../../backend/src/cache/runtime.rs) | BE-01；与 BE-03 共同核验 fingerprint |
| BE-05 | 详情日分片和提交读口 | [storage service](../../backend/src/storage/service.rs)、[sqlite](../../backend/src/storage/sqlite.rs)、[writer](../../backend/src/storage/writer.rs)、[management_read](../../backend/src/storage/management_read.rs)、[storage port](../../backend/src/ports/storage.rs) | BE-01、BE-03 |
| BE-06 | 统一保留协调器 | [stats](../../backend/src/storage/stats.rs)、[statistics](../../backend/src/storage/statistics.rs)、[ledger](../../backend/src/storage/ledger.rs)、storage service | BE-05、D-06/D-08 |
| BE-07 | 配置与观测查询 | [management port](../../backend/src/ports/management.rs)、[query](../../backend/src/management/query.rs)、management_read、[router](../../backend/src/management/router.rs) | 配置读依赖 BE-02；历史读依赖 BE-03/05/06 |
| BE-08 | 模块级受限写接口 | router、ConfigStore、source_edit、config validate/resolve | BE-02、BE-07 配置读；按模块接入 BE-03/04/06 |
| BE-09 | 实时指标与进程采样 | [DNS service](../../backend/src/service.rs)、[telemetry port](../../backend/src/ports/telemetry.rs)、query、management server | BE-01、D-04/D-08 |
| BE-10 | WebSocket 与补齐 | management router/server、management port、详情提交事件、指标采样 | BE-05、BE-07、BE-09、D-05/D-08 |
| BE-11 | 新基线初始化与旧路径退出 | config load/migrate、[migrations](../../backend/migrations)、storage service、app | 初始化从 BE-01 开始；旧路径退出依赖新接线 |
| BE-12 | 组合、安全和负载验收 | 各模块就近测试、[backend contract tests](../../backend/src/storage/backend_contract_tests.rs)、[交付入口](../implementation/delivery.md) | BE-01 至 BE-11、前端联合验收 |

表中链接为现有入口，不表示这些文件已实现目标能力。新增源码文件仅在已有模块内部按真实职责拆分，例如详情分片、保留或 WS adapter；不预建新的顶层架构层。

## 3. BE-01：冻结版本和类型契约

P0 进度：BC-01 的配置内部契约和 API/跨端契约两个语义单元已落实。配置单元提交为 `166e59e`（`refactor(config): 定义新版配置与校验契约`）；字段和边界见[配置参考](../implementation/configuration.md#v2-契约与生产基线2026-09-08)，API/状态/预算及测试见[Management 实现](../implementation/backend/management.md#p0-v2-契约)。生成类型和 12 路由/表单契约可供后续消费，但没有新页面或 v2 handler 接线。

BE-01 中“新 fixture 可直接启动”的联合验收依赖 BC-26；当前 fixture 仅通过离线新契约解析，不能作为生产启动成功证据。BC-01 契约交付不等于 BE-01/BC-26 的生产切换和完整验收全部完成。P1 已于 2026-09-07 获得授权；BC-02 内部能力、BC-03 的差量 socket/任务预注册子项和 BC-23 指标已推进，进度见下文，不重复实施 P0，也不扩大到 P2。

配置单元验证（Windows，2026-09-07）：Rust 1.98.0 / Node 26.8.1 / pnpm 11.25.0；`cargo test --manifest-path backend/Cargo.toml config:: --bin fluxdns` 53 通过，含 7 个新增 v2 测试；`cargo fmt --manifest-path backend/Cargo.toml -- --check`、前端 `pnpm run typecheck`、文档检查器和 `git diff --check` 通过。项目只有 binary target，最初 `--lib` 调用已修正；夹具的规则/mode 和裸 null 解析问题已回归通过。未运行新格式生产启动、磁盘 alias 防护、浏览器或性能测试。

跨端单元验证（同日同工具链，Windows）：后端 `management:: --bin fluxdns` 18 项、`config:: --bin fluxdns` 53 项通过；`cargo test --manifest-path backend/Cargo.toml --all-targets --no-run` 编译全部测试目标通过，未执行全量 Cargo suite；Rust fmt check 通过。前端 `generate:api` 连续两次 SHA-256 一致，当前 v1 产物无 diff；`typecheck`、`test:contract:v2` 3 项、`test` 7 文件 38 项及 `build` 通过。Node/Vite 子进程在沙盒内曾被 EPERM 阻止，经批准外部重跑通过；Vitest 的 Node localStorage 实验性警告不影响结果。文档检查器与 `git diff --check` 通过。定向复核修复了路径反向父子冲突、WS 筛选绕过 REST 校验及 schema 的 u64 上界；不把既有 HTTP 测试当作新版 handler/WS 验收，也未执行 BC-26 启动、P5/2ms、跨平台或浏览器验收。

### 开发步骤

1. 形成总计划决策记录，确定配置/API 主版本及旧请求失败方式。配置版本、统计 DB schema、详情 layout、缓存格式和 WS 协议版本分别管理。
2. 更新 DTO/归一化/校验：各模块必填唯一 `name`；客户端另有单个唯一 `client_id` 及 IP 匹配；快照 `enabled/path/snapshot_interval`；`statistics.retention`；`database.records_path`；详情配置仅 `enable`。
3. 明确源值与生效值、未设置与显式禁用的类型表示，沿当前配置校验处理三态，不能凭空把所有缺失改成 `false` 或 `null`。
4. 定义资源读取/预校验/应用保存、active/file revision、旧 name、operation 查询、外部差异/还原/组合采用和重试同步；payload 使用严格模块类型，不接收任意 JSON Patch 或整份 YAML。
5. 定义身份 DTO、稳定记录 ID、历史分页 cursor、提交序列 cursor、保留预览/状态、指标单位与可用性。WS 消息体的 schema 与 HTTP DTO 共用唯一类型权威，不另手写一套前端事件模型。
6. 明确有效载荷、分页、天数、查询时间、WS 帧/速率/队列、在线身份数量等保护值及超限错误。详情条数业务配额删除不等于取消资源保护。
7. 同批更新正式 OpenAPI、类型生成脚本输入和生成类型、合法/非法 fixture；不手工编辑 `generated.ts`。API 切换同步考虑 auth client、Vite 开发代理、mock、SPA API fallback。

### 交付与验收

- 每个字段能追溯到需求、图稿或现有 schema；不存在虚构 listener 开关、额外上游协议、缓存持久化配额。
- 严格解析测试覆盖未知字段、类型分支残留、单位/溢出、非法 CIDR、路径别名/碰撞、缺失/禁用与引用环。
- 新版本 fixture 由 BC-26 正式 loader 直接解析和启动；旧格式明确拒绝，不实现旧配置转换预览或猜测映射。完整浏览器、Linux 与真实故障介质仍单列为未验收边界。
- FE-01/FE-02 可使用生成类型开始开发，但不把 fixture 当作正式 handler 已就绪。

## 4. BE-02：配置事务、revision 与运行时应用

BC-02 内部进度（2026-09-07）：ConfigStore 已有 v2 活动源、双文件有界观测、版本分离、调用者/候选/双版本绑定票据、有界幂等操作和中断阻塞；定向源编辑已覆盖类型化引用、组合候选完整语义/路径校验及编辑后等价核对。实现与测试权威见[配置参考](../implementation/configuration.md#p1-活动源与候选内部底座2026-09-07)。尚未提供正式 v2 loader/resolve、候选到资源/socket prepare 的转换、状态端点或新版 setup；应用回报测试仅为状态机模拟。BC-03 已有[有界服务队列消费者](../implementation/backend/lifecycle.md#p1-服务控制队列子项2026-09-07)，但未连接 v2 活动源/operation 生产者。BC-29 的[分阶段文件事务](../implementation/configuration.md#p1-应用后持久化内部底座2026-09-07)已接入活动源内部状态机并完成 Windows 子进程 crash point/文件失败重试测试；还原和外改重新确认已有内部能力，正式启动恢复和 HTTP 仍未闭合。BC-30 已交付正式 app 的[仅提示 watcher](../implementation/backend/lifecycle.md#p1-仅提示文件观测2026-09-07)，双文件读取移出同步控制循环，真实 DNS 外改隔离及 Hosts 自动刷新有定向证据；[冻结结果/状态投影](../implementation/backend/management.md#p1-配置状态内部投影2026-09-07)和逐文件自写身份识别已有内部消费。差异预览、异步事务 owner、状态/还原/重试端点及 UI 仍未完成，因此不能关闭 P1 或 BC-30。

2026-09-08 BC-30 补充：固定源同次读取、完整校验、双文件/活动版本复核及十模块有界类型化差异已有[内部实现](../implementation/backend/management.md#p1-外部配置差异内部投影2026-09-08)。后续差异工作从异步事务 owner、鉴权/handler 和前端组合采用接线继续，不以内部文件测试关闭 BC-30。

2026-09-08 认证追加决定已在当前 v1 生产链实施：业务接口只接受 Bearer，Cookie 仅用于认证刷新；当前/目标 schema、client 与 mock 已统一，真实 HTTP/浏览器证据见[Management 实现](../implementation/backend/management.md#p1-bearer-业务鉴权2026-09-08)。这不是 v2 配置接口注册，BC-30 的异步 owner/handler 和 BC-26 依赖不变。

完整状态机、热更新矩阵和失败语义只维护于[配置热更新专项](webui-management-config-runtime-plan.md)，本节列后端开发步骤：

1. 在 ConfigStore 保留活动源表达及 active/persisted/file revision；GET 和普通编辑以该源为基准，保留路径/继承/SecretRef，不序列化 resolved 值写回。
2. 向持有 DnsService 的控制循环提交有界 typed 命令；复用 RuntimeCoordinator 的候选和 mutation gate，prepare 与最终提交分开，提交时复核版本。
3. 扩展 `reload_prepared`：差量复用、任务预注册和已接纳请求按原 deadline drain 已接入，见[生命周期实现](../implementation/backend/lifecycle.md#p1-请求-drain-子项2026-09-07)。继续连接活动源/operation 生产者、新进程 owner 可失败准备/真实补偿。不能将这些内部子项测试等同于完整不停机应用验收。
4. 先应用后正式文件替换的内部状态机、PREPARED/COMMIT_DECIDED journal 与已知状态下文件重试已实现；继续完成服务成功回报、启动恢复及正式状态/重试端点。不能以模拟 Runtime 回报关闭联合验收。
5. 将 app watcher 改为只检测和上报；复用去抖轮询但有界读取，区分自写/外改/不可读/缺失；不改变 Hosts/规则集资源刷新。
6. 已有[受管文件还原内部能力](../implementation/configuration.md#p1-受管文件还原内部能力2026-09-07)和[外改确认重试](../implementation/configuration.md#p1-外改确认重试内部能力2026-09-07)，绑定 active/file 双版本、调用者及 operation，覆盖缺失叶节点的权限保持重建及新旧 journal 决策交接；继续实现脱敏差异、模块化组合采用、普通保存覆盖确认与状态/还原/重试端点，不接收任意文件路径或整份 YAML，不据此关闭 BC-30。
7. [日志 owner](../implementation/backend/background-services.md#p1-日志热切换2026-09-07)已接入 app/service，复用 reload handle、共享输出和 writer，具备 off/on、level/path、单槽后台预开和 filter/发布失败分类；真实 UDP/SQLite 联合测试验证持续 DNS 与进程指标复用。继续完成 v2 配置事务生产者、应用后持久化和 HTTP/UI 日志保存的联合接线，不以当前 v1 service 证据关闭 BC-31。
8. 首用户初始化继续满足“持久化用户成功后发布认证”的安全要求，但写入仲裁和活动源必须与普通配置协调；本期不新增用户管理 API。普通配置/用户排序不回收 session 的显式应用边界已有 [P1 回归](../implementation/backend/management.md)，文件仅提示与 v2 setup 接线仍保留。

### 验收

专项 CR-01 至 CR-06 在 Windows 的文件/runtime/owner 故障矩阵必须完成；每个实现检查点带定向测试，BC-32 补跨模块交错。运行时优先不代表所有 I/O 都可原子回滚，接口如实表达已应用未同步和补偿失败。

## 5. BE-03：客户端身份贯穿请求与投影

2026-09-08 BC-04/BC-05 内部进度：v2 配置已冻结唯一 `name`/单 `client_id`，现有 resolved/policy 类型进一步显式区分管理 `name` 与请求 `client_ids`；`ClientIndex` 提供独立 name/exact ID 索引、重复拒绝、ID 优先/最长 CIDR 和 mapped IPv4 归一化，事实见[配置参考](../implementation/configuration.md#p1-客户端匹配索引内部能力2026-09-08)。transport 原始 `client_id`/IP、请求期 `Id`/`Ip` 匹配来源和稳定 ID 已经由完成事件冻结，详情 schema v7 持久化并由统计直接归属，reload 后不重映射；事实见[后台服务](../implementation/backend/background-services.md#完成事件与后台分发)。BC-26 已把单 `client_id` 正式接入生产 resolver；旧历史仍不补造或重匹配。日分片查询与对外投影分别由 BC-09/BC-13、BC-22 承接。

### 开发步骤

1. 在 [UDP](../../backend/src/transport/udp.rs)、[TCP](../../backend/src/transport/tcp.rs)、[DoH](../../backend/src/transport/doh.rs) 到 `RequestContext` 的路径核对原始 ID/IP 捕获。无 ID 协议保持 `null`；DoH 仅接受现有可信来源的有效 IP。
2. `name` 作为客户端配置管理键且唯一，单个 `client_id` 作为请求匹配/历史归属身份且唯一；构建 name 和 ID 索引，保留 ID 优先与最长 CIDR 回退、IPv4-mapped IPv6 归一化。
3. Policy 在当前请求 runtime 中冻结 `matched_client_id` 与来源，不增加第二次匹配；经完成事件和 detail projector 写入原始身份与最小匹配结果。
4. 明确无原始 ID、未匹配、Answer 截断/不可用等新记录状态；删除专为旧格式设计的 identity/detail legacy 兼容状态。
5. 新统计按匹配 ID 或有界 unknown 维度归属；事件消费/reload 不重映射，不读取旧名称维度。
6. 缓存保留 ID 命中按实际 ID、IP 命中按实际 IP 的域分隔隔离，结合生效策略；同 CIDR 的不同 IP 不能合并为同一个客户端池。
7. 控制新增 ID 字段的长度、序列化和日志边界；请求原始 ID/IP 不得变成无界 telemetry labels。

### 验收

以原方案的[请求期匹配矩阵](webui-management-backend-refactor.md#43-请求期匹配矩阵)为测试数据表，覆盖 ID/IP 冲突、未知 ID 回退、重复 name/ID 拒绝、CIDR 包含/重复、配置消失/ID 复用及 reload 交错。校验真实存储列、统计归属和缓存隔离；无 ID 不能被 IP 匹配结果补造。

交付 BE-05 的详情模型、BE-07 的查询投影及 FE-04/FE-10 的 typed fixture。

## 6. BE-04：内存权威与独立缓存快照

2026-09-08 BC-06 已完成：新增独立 `FDCS` 二进制完整快照、Moka 有界分批导出、header/body SHA-256 和先完整校验后分批恢复 reader；真实 Windows 临时文件覆盖停机 TTL、预算、损坏和失败保留旧文件。

同日 BC-07 已完成：正式 app 在 bind 前启动唯一 `CacheSnapshotOwner` 并恢复活动 Moka，使用固定 5 分钟过渡周期完整覆盖；coordinator 在 reload 提交后切换 source/generation，service 在 late finalizer 后用剩余 deadline 最终写入。生产 `PolicyDnsCore`/prepare 不再创建或挂接 SQLite cache persistence，旧 adapter 仅留契约测试等待 BC-27。真实文件覆盖跨 core 重启、缩小内存预算、周期写入/跳过、reload/clear 旧代发布拒绝与清理后不复活、路径 alias 和有界 shutdown。当前仍是 v1 loader 提供路径，v2 cache 字段加载及新数据基线启动仍由 BC-26 完成。

BC-07 Windows 验证：全量 Cargo suite 807 passed、0 failed、3 ignored；定向 service、app、coordinator 和真实 `FDCS` 重启链路均通过。fmt、全部测试目标编译、文档与 diff 检查通过。未执行三个手动/大连接 ignored 专项、Linux、完整 v2 冷启/重启、真实权限/磁盘满、浏览器或约 10 客户端及 core 2ms 性能验收。

同日 BC-08 已完成生产写入接线：`StorageRuntime` 将统计主库与 `detail_shards` owner 分离，详情按事件 UTC 日写入 layout v1 的 `YYYY-MM-DD.sqlite3`，registry 提供同日串行、最多 4 个活动连接、读写/退役 lease 与有界 shutdown。生产 batch 不再执行旧单库详情的历史 `COUNT`、按条数/年龄 `DELETE` 或 `VACUUM`；v1 三个配额字段仅待 BC-26 删除 loader 契约，主库旧详情 adapter 留到 BC-27。BC-08 不迁移旧记录；过渡目录由统计库同级 `queries/` 推导，正式 v2 `database.records_path` 仍归 BC-26。真实 SQLite 已定向覆盖跨日/迟到、错误日 trigger、只读不建库、外部库拒绝、连接/退役/关闭交错和小 v1 配额不截断；跨分片 ID/cursor/通知继续由 BC-09 完成，共同水位/物理回收由 BC-10/11 完成。

BC-08 Windows 验证：完整 Cargo suite 815 passed、0 failed、3 ignored；Storage 定向 75 项和全部测试目标编译通过，真实文件另覆盖硬链接拒绝。fmt、文档与 diff 检查通过。未执行三个 ignored 专项、Linux、完整 v2 冷启/重启、真实权限/磁盘满、跨日 cursor/回收、浏览器或约 10 客户端及 core 2ms 性能验收。

同日 BC-09 已完成 storage 基础：layout v1 前向增加耗时和身份/qname 查询索引；稳定 opaque ID 编码 UTC 日与事务返回的本地 row ID，跨重启不变；keyset cursor 绑定 filter/sort/order/direction、retention revision 与进程 key。跨分片查询逐日取得只读 lease，在 SQLite 内先过滤和应用 keyset/`page_size + 1`，再做有界全局归并，不执行 `OFFSET`/`COUNT`。独立 `stream_epoch + sequence` 在详情事务 commit 后才发布批记录通知，迟到事件按提交顺序可见，失败/丢弃不提前通知。当前客户端名称/目录 revision 和正式 Bearer HTTP 留 BC-13，replay/WS 留 BC-25，共同水位持久化仍留 BC-10。

BC-09 Windows 验证：真实 SQLite 新增 7 项、detail 定向 15 项通过；完整 Cargo suite 822 passed、0 failed、3 ignored。覆盖跨日/同毫秒前后分页、复合过滤、duration 排序、ID 重启一致、cursor 篡改/上下文/进程/水位失效、按 ID 读取、空读不建库、deadline 和 commit 前后/失败通知。未执行三个 ignored 专项、Linux、完整 v2 冷启/重启、真实权限/磁盘满、共同水位/回收、Bearer HTTP/WS、浏览器或约 10 客户端及 core 2ms 性能验收。

同日 BC-10 已完成共同水位基础：R/G/T 纯计算在 `S == T` 时保留宽限并包含当前 UTC 日；大小只采样受管详情主文件与 WAL。schema v8 在同一 stats 事务内单调发布水位、清理旧统计、按进程冻结的 replay floor 回收 ledger，并登记旧详情日 manifest。全局详情 retention lease 先阻断新 lease 并排空在途读写，事务成功后才发布逻辑水位；失败不提前隐藏。stats pending 重放只推进幂等确认，详情迟到批次计为 dropped，`StorageRuntime` 启动恢复水位并续接 batch ID。01:00 调度、补跑、状态/预览确认及物理文件回收仍留 BC-11，v2 R/G/T 生产配置入口仍不冒充 BC-26。

BC-10 Windows 验证：真实 stats SQLite/日分片新增 4 项 retention 用例及 1 项 `StorageRuntime` 重启用例，storage 定向 87 项通过；完整 Cargo suite 827 passed、0 failed、3 ignored，全部测试目标编译通过。覆盖计划表中的阈值/边界、受管大小口径、水位前后失败、lease 排空、pending replay、迟到写、manifest/ledger 和重启恢复。未执行三个 ignored 专项、Linux、完整 v2 冷启/重启、真实权限/磁盘满、01:00/时区/DST、物理删除重试、Bearer HTTP/WS、浏览器或约 10 客户端及 core 2ms 性能验收。

同日 BC-11 已完成唯一生产 retention scheduler owner：每分钟通过 Jiff 重读服务器时区，按本地 01:00 单日执行；新空库建立有时间语义的基线，既有水位启动补跑，失败五分钟重试，DST 跳时/重复、墙钟回拨和时区日期变化由持久化 `retention_run_state` 去重。物理回收逐日等待 retirement lease，验证 layout、checkpoint/关闭后仅删除受管主文件及 SQLite sidecar；失败保存 manifest attempts/安全错误码并重试。只读 storage 状态已提供目标天数、已发布/预计水位、实际 stats/detail 范围、清理时间、空间大小和 pending/failed 计数。当前 owner 固定消费确认默认 R/G/T；正式 v2 typed 配置仍归 BC-26。

BC-11 Windows 验证：7 项 retention、91 项 storage 定向通过；完整 Cargo suite 830 passed、0 failed、3 ignored，全部测试目标编译通过。真实 stats SQLite 与三日详情分片覆盖删除失败、manifest 重试/reclaimed、状态范围和 cache 文件隔离；受控墙钟覆盖 01:00、DST/回拨/时区变化、retry gate 与重启。Clippy 未新增告警，仍被基线已有 6 项 lint 阻断。未执行三个 ignored 专项、Linux、真实权限/磁盘满、发布 binary 跨 DST 长时间运行、完整 v2 冷启/重启、Bearer HTTP/WS、浏览器或约 10 客户端及 core 2ms 性能验收。

同日经用户追加授权，BC-26 已提前实施：`ConfigV2Loader` 直接生成运行态并保留原始活动源，根示例/fixture 切为 v2；生产 cache、records_path 和 retention owner 分别消费 enabled/path/interval、日分片路径和 R/G/T。统计 SQLite 用事务化 `fluxdns_layout` 标记区分新布局，空库可初始化/重开，已有未标记旧库和旧配置明确拒绝。Management 绑定 active `ConfigStore`，一次性 setup 在同一活动源上更新双文件与认证事实；普通 P3 写入仍未开放。旧 loader、单库详情和 SQLite cache adapter 只留测试，待 BC-27 删除。

BC-26 Windows 验证：真实 `StorageRuntime` 临时文件覆盖空目录初始化、详情写入、正常关闭和重开，统计布局测试另覆盖旧库拒绝；完整 Cargo suite 835 passed、0 failed、3 ignored，全部测试目标编译和 fmt 通过。本地构建二进制在独立 `_fluxdns/bc26-process-*` 目录以启用 WebUI 的 v2 配置完成冷启与终止后重启，均确认 Management 端口、派生快照和统计库存在；CLI v2 validate 返回 0，v1 返回 2 并提示新开发目录。前端生成类型无 diff、typecheck 通过，v2 schema 4 项在沙盒外通过。Clippy 仍仅被基线已有 6 项 lint 阻断；未验证 Linux、真实权限/磁盘满、完整浏览器、约 10 客户端及 core 2ms 性能。

### 开发步骤

1. 审核 `cache/persistence.rs` 的现有 codec，保留可复用的版本、TTL、fingerprint、canonical wire 与 provenance；替换其第二份全量集合和文件容量逻辑，不直接将旧 adapter 改名接入。
2. 为共享内存缓存提供有界分批遍历，包含全局/策略/客户端池。释放查询锁后编码，限制单条长度和并发快照任务，不全量复制缓存再生成大 buffer。
3. 实现完整快照头、版本、生成时间、长度/校验；同目录临时写入、flush/sync、发布仲裁和完整文件替换。失败保留上一份可用快照。
4. 增加进程级快照 owner，定期覆盖，无变化可跳过；reload 重用/切换 owner，旧 epoch/generation 不能覆盖新文件。移除生产装配中的 SQLite 缓存增量写队列。
5. 首次启动有界流式预热，逐条检查预算、有效期、版本、fingerprint。停机时间扣减 TTL/乐观期限；损坏或超时允许冷启，不改业务数据。
6. 对内存清理/所有权切换定义持久化失效边界与 reset generation；验证旧文件不会在清理后复活。本期不新增清缓存 UI/API。
7. 正常 shutdown 使用统一剩余预算尽力补写；关快照不关内存，关全局池不等于关全部逻辑池。
8. 公开只读快照/恢复状态、文件大小、最后成功/失败时间和安全错误，不返回 wire；路径校验覆盖业务库、分片目录、符号链接/重解析点和临时文件归属。

### 验收

真实临时目录测试开关、缺失/损坏、未知版本、权限/空间失败、恢复超时、预算缩小、停机 TTL、并发淘汰、clear/reload/shutdown 交错及 Windows 替换。Linux 特有实现做源码审查，不要求另行实测。检查导出短锁和有界 I/O，不把内存预算宣称为磁盘硬配额。

生产启动路径不再创建缓存 SQLite；统计库/详情文件保持不变；FE-07 能显示成功、部分恢复、冷启与失败原因。

## 7. BE-05：UTC 日分片详情存储

### 开发步骤

1. 定义详情 layout version、日期路径解析、受管理分片目录及索引。新库路径与统计库/缓存路径严格分离，文件名只来自已解析 UTC 日期。
2. 拆分业务统计 backend 与 detail writer 的所有权，保持现有有界事件/详情队列和非阻塞发布。详情仅按事件日写入，禁止每个历史日常驻连接。
3. 批写事务只做有界校验与 INSERT，不再调用历史 COUNT、按条数淘汰、历史 DELETE/VACUUM；业务条数上限删除后仍有批次/队列/响应保护。
4. 增加分片 registry 和读写 lease、受限活动连接与关闭流程；为 BE-06 提供禁止新写入、排空、checkpoint/关闭、退役和恢复入口。
5. 定义含分片定位信息的不透明稳定记录 ID，排序为事件 UTC 毫秒加稳定 ID；keyset cursor 带过滤/排序上下文，不把单库自增 ID 当全局 ID。
6. 按时间范围定位日期集合，在各分片先应用过滤和索引，再合并有界结果。总数单独按相同过滤聚合且有超时；不能使用“当前页长度”冒充总数。
7. 按已确认 D-05 使用前后 cursor，不实现任意页码直跳或无界 OFFSET。
8. 为 BE-10 提供“详情事务提交后”的记录通知。提交序列与事件发生时间分离，迟到写入也能被订阅者发现；失败/丢弃的详情不得先推送成可查询记录。
9. 详情关闭时保留历史读能力；热关闭与重新开启由进程 owner 控制，不重新创建统计服务。

### 验收

覆盖空目录、跨午夜、乱序/迟到、相同毫秒记录、双向翻页、复杂过滤、总数超时、受限连接数、只读查询不创建新库、非法 cursor、持久化失败、详情开关交错。必须在真实 SQLite 中验证分页不重不漏和批写 SQL，不以 memory adapter 替代。

水位检查和退役阻止迟到重建由 BE-06 完成；BE-05 单独通过不代表保留功能已可交付。

## 8. BE-06：保留协调器与共同水位

### 开发步骤

1. 将 R/G/T 的合法值、大小口径、调度时区和下一执行时间接入 typed 配置；UTC 事件日与服务器时区任务日分别表示。
2. 实现可独立测试的保留计算与预览。沿原方案 `S > T` 取 R，否则取 R+G，包含当前 UTC 日，删除严格早于截止日的数据；等于阈值仍享受宽限期。
3. 采样受管理详情主文件和 WAL 长度，不计统计库/缓存/SHM/备份；冻结本轮 revision、S 和截止线，采样失败则本轮失败，不按 0 处理。
4. 在统计事务中发布单调水位和任务 manifest，并处理过期统计日；成功后才回收详情。reader、writer 和 stats pending retry 都遵守同一水位。
5. 详情分片先拒绝新增 writer，再有界排空 lease、关闭/checkpoint、删除确切归属的主文件及 sidecar；失败标为待回收并重试，不删除仍打开的文件。
6. stats pending batch 对已退役日期跳过业务增量但推进正确幂等确认；ledger 按已确认 replay 下界回收，不按统计日期粗暴删除。
7. 实现每天服务器时间 01:00 的进程级调度与单次补跑；覆盖夏令时跳过/重复、回拨、时区变更和进程重启。定时器重新计算墙上时钟时间，不固定 sleep 24h。
8. 缩短期限预览同时绑定 expected revision、当前水位与输入参数；确认后只保存策略，下次任务重新采样。增大期限/压力下降不回退水位。
9. 查询端分别输出目标天数、已发布截止线、实际可查日期、下次预计截止日、清理时间、失败/待回收状态和空间信息。详情关闭仍执行统计保留。

### 验收

表驱动覆盖 R=1/3/7/30/自定义、G=0、S<T/S=T/S>T、截止日等号和整数溢出；受控时钟覆盖不同服务器时区与 UTC 日期。故障注入覆盖发布水位前后中断、删除失败、查询 lease 超时、迟到写入、pending 重放、manifest 重启恢复及采样错误。

至少完成一次真实统计库加多日详情库回收演练，确认逻辑同时不可见、物理失败可追踪、缓存未受影响，且无有效期内数据因空间压力被删除。

## 9. BE-07：管理查询与安全配置投影

### 9.1 查询交付清单

| 主题 | 必须返回的能力 | 生产数据来源 |
| --- | --- | --- |
| 各配置模块 | 活动源字段、类型、引用、active/file/persisted revision、继承来源及同步状态 | ConfigStore 活动源 + runtime/资源快照，按同次读取边界标识 |
| 监听入口 | 逻辑入口、多地址绑定、DoH routes/endpoints、真实绑定状态和 ECS 来源 | 配置及 runtime bind 信息；不存在独立 enable 时不伪造 |
| DNS 上游/组 | 当前配置类型、连接/成员/模式/回退、被引用位置 | 同一配置 revision 的类型化引用图 |
| DNS 配置 | 全局缓存/TTL/ECS、R/G/T、详情开关及快照/保留状态 | BE-04、BE-06 的只读状态 |
| Hosts/规则集 | 源类型、格式、内容或来源、更新计划与有效/陈旧状态 | 源配置与 resource metadata，内容只读取批准的内联配置 |
| 客户端 | 唯一 name 管理键、单 client_id、IP/CIDR、策略/覆盖与目录 revision | 单次当前客户端目录快照 |
| 代理/系统配置 | 批准的 SecretRef 来源与系统源路径、logs、只读 database/webui/work | D-07 白名单投影，禁止 Secret 实际值与 users/hash |
| 解析记录 | 全部原始/历史/当前显示信息，Answer/provenance、过滤、详情定位和跨日分页 | BE-05 分片读口 + BE-06 水位 + 当前目录 |
| 服务/进程信息 | 采样时间、单位、窗口、可用性及数值 | BE-09 共享进程采样器和请求观测 |

### 9.2 查询实施要点

1. 扩展 `ManagementStorageRead` 和领域结果，不把 SQLx/HTTP 类型引入核心 port；全部过滤采用固定模板/绑定参数。
2. 原始 ID/IP、匹配 ID、域名、协议、来源、响应码、结果状态、时间和排序在分页前过滤。名称模糊搜索先解析完整当前 ID 集合，再过滤历史匹配 ID；精确名称唯一。
3. 一页关联一次当前目录，不逐行查询。原始值与匹配结果保持历史事实，当前名称、配置是否存在及 directory revision 单独返回。
4. 保留新记录所需有界 Answer、truncated 数量、耗时缺失、实际/目标上游及缓存 producer provenance；删除只服务旧 schema 的 legacy 分支，不丢有效的新记录语义。
5. 默认 R 天，允许显式访问尚可查宽限日；大于 31 天窗口用分段聚合/有界扫描和明确超限结果，删除旧硬编码限制而非默默截断。
6. 区分 unavailable、空结果、记录不存在、已超出水位、cursor 过期和 deadline。保留清理与查询交错时检查水位版本，必要时要求重取快照，不返回失效 cursor 伪装成最后一页。
7. 配置 GET 不等于文件浏览：只返回 D-07 批准的源字段，不打开任意传入路径，不回显进程解析后的绝对路径、秘密内容或全量 resolved config。

### 验收

HTTP 契约对照实际 JSON 与 OpenAPI；单次名称快照、模糊名称筛选、改名/配置消失/ID 复用、分页前筛选、日期水位和大窗口测试通过。未认证/错误响应和日志中不能出现配置秘密或查询原文泄漏。

## 10. BE-08：逐模块写接口

### 10.1 通用要求

每组提供读取、预校验及应用保存能力。所有写入使用 BE-02、严格 DTO、active/file revision 和旧 name。普通模块端点拒绝混入其他模块字段；外部组合采用只接收受控模块命令列表，统一完整候选校验，不开放自由补丁。正式字段由 BE-01 写入 OpenAPI。

新增/编辑只覆盖图稿已有范围；策略规则、组成员、DoH 路由等子项通过完整资源候选编辑。顶层删除、任意文件覆盖、手动资源刷新和系统操作不属于本期端点。

当前 router 的 16 KiB body 上限是认证/查询基线，不直接适配所有内联配置。为确有需要的配置端点设置经评审的有界上限，保持 auth 更小限额，校验实际 body 与声明长度，仍受源 writer 大小边界限制，不能全局放大或取消保护。

### 10.2 模块工作包

| 子任务 | 开发内容和关键校验 | 生效/依赖 | 必测样例 |
| --- | --- | --- | --- |
| BE-08A Hosts | const/file 与 json/hosts 分支；结构化域名多 IP；来源切换只提交当前有效字段，文件仅修改引用路径/更新参数 | prepare 资源，引用校验 | 重复域名聚合、非法 IP、文件不存在、首载失败和陈旧旧版 |
| BE-08B 规则集 | const/file/remote、json/clash/dat 合法组合；proxy、周期、selector；复用实际 parser | 代理已存在；prepare/刷新 owner | 行格式不当 YAML、dat 非文本、URL 失败、selector 缺失 |
| BE-08C 代理 | 名称/type/SecretRef env 或 file；仅编辑引用，不编辑解析值，不额外增加解析模式开关 | 下游 connector 候选准备 | env/file 互斥、引用缺失、秘密不回显、被引用改名 |
| BE-08D 上游/组 | 当前 hosts/doh/group 类型；DoH address/bootstrap/connect_ip/proxy/ECS；group 主/回退成员、模式、超时/权重 | 资源/代理准备后推进 | 旧名->新名、嵌套/循环引用、模式切换残留权重、类型切换 |
| BE-08E 策略 | default upstream、有序 rules、hosts/rule_set 互斥、upstream 与 ECS；cache/TTL/ECS 继承/覆盖 | 上游/资源就绪 | 第一条命中、移动顺序、Hosts 本地回答不填上游、禁用与继承区分 |
| BE-08F listener | UDP/TCP 多地址/端口/策略/Hosts；DoH 共享路由与独立 endpoints/TLS/client IP | 策略就绪；bind preflight | route 重叠、端口冲突、双栈、TLS 分支、可信代理及错误策略 |
| BE-08G 客户端 | 新建唯一 name/ID、按旧 name 编辑名称/IP/覆盖；普通编辑不接受 ID 改写 | BE-03 + 策略 | 重复 name/ID、空 IP、规范化 CIDR 冲突、原始无 ID |
| BE-08H DNS 配置 | 缓存、TTL/ECS、statistics retention、详情 enable 分区受限保存；R/G/T 一次提交 | BE-04/06 + 热开关 owner | 预算/周期单位、缩短预览确认、任务边界 revision、详情关闭统计不停止 |
| BE-08I logs | 仅 enable/level/path 热更新；其余系统字段严格只读 | BE-02 热日志 owner | 注入只读字段被拒；日志关开、级别/路径生效和落盘失败 |

上游及组改名按类型遍历所有引用，包括其他 DoH bootstrap、组主/回退成员、策略默认/规则上游；相关资源改名同时处理 listener hosts/策略、客户端策略、rule_set selector 的资源部分和 proxy 引用。禁止对 YAML 原文全局字符串替换，不能把 `geosite:cn` 的 selector 部分或自由文本误改。

组模式延续既有规则：parallel/failover 不接受可编辑权重，round-robin/load-balance 才使用权重；DNS 终态响应不能一概当作触发 fallback 的传输失败。配置编辑不修改核心算法。

### 验收与交接

每个子任务提供正常应用保存、字段错误、revision 冲突、引用失败、prepare/应用失败、落盘失败及查询回显测试。FE 完成真实热更新/同步状态和外部模块差异采用后才能完成；mock 编辑成功不能关闭任务。

## 11. BE-09：服务指标和进程信息

### 口径确认入口

QPS/RPM、趋势、在线身份、暖机和内存单位按[已确认 D-04](webui-management-decisions.md#d-04-指标口径与在线客户端)纳入正式契约；采样实现和容量按 T-05 核定，不在本节另存参数表。

### 开发步骤

1. 选择接入计数边界并防止 TCP 多请求、重试、single-flight 和迟到上游重复计数；实现有界秒/分钟桶。
2. 在线身份从已验证请求上下文捕获，与详情开关解耦。仅保留满足 60 秒窗口的有界脱敏 key，不将原始 ID/IP 写入统计库或 telemetry label。
3. 在线身份容量达到上限时显式返回不完整/不可用原因，不能把截断后的数量宣称精确；已覆盖完整窗口但没有请求才可报告真实 0。
4. 进程启动不足 60/600 秒返回覆盖时长和 warming 状态，正式完整窗口数值未就绪时不把停机区间填零。采样失败与观测丢失返回缺口标记。
5. 进程采样器单 owner 定期更新，各页面/WS 读取共享快照；不得每个订阅连接启动独立 OS 采样。
6. 验证 Windows 进程信息及时区，Linux 特有代码检查接口与条件编译但不要求本轮实测；采样错误局部降级，DNS 请求不等待采样。

### 验收

受控时钟与已知请求序列核算窗口、趋势和在线身份；覆盖 NAT、同 IP 不同 ID、unknown ID、详情关闭、暖机、缺口、采样失败及容量上限。Windows 真实采样，Linux 特有实现标明未实测；两处内存口径一致。

2026-09-08 BC-23 已完成：生产 service 在统一接纳边界记录 UDP/TCP/DoH 请求，进程级 owner 提供固定窗口、4096 在线身份保护和每秒共享 OS 快照，并注册两个受 Bearer 保护的 v2 查询端点。受控窗口、跨 transport、容量/缺口、Windows 真实采样及生产 loopback HTTP 已验证；Linux 条件编译实现、浏览器、性能及 BC-24 WebSocket 未在本项验收。

## 12. BE-10：WebSocket、序列与断线补齐

### 开发步骤

1. 在独立 Management Axum adapter 上启用经批准的 WS 能力；复用同一会话权威，保持 Origin 校验与受监督生命周期，不复用 DoH parser。业务只使用 Bearer，不以 Cookie 或 URL query 传 token；浏览器原生 WS 不能直接设置 Authorization header，凭据传递与握手方案在 BC-24 实施时核定，不由当前 HTTP 认证代替验收。
2. 定义服务状态和解析记录订阅，推送周期采用[决策清单 D-05](webui-management-decisions.md#d-05-分页自动刷新与实时缓冲)确认的值；实现为有界默认行为，不擅自新增 YAML 调优字段。
3. 每个连接限制订阅、帧长度、发送队列和速率；慢消费者收到缺口/重同步信号或断开，DNS 和 detail writer 不等网络发送。
4. 身份过滤与 HTTP 查询语义一致。记录只在 BE-05 提交后发布，使用 `stream_epoch + sequence`，不能以事件时间作为“新记录”依据。
5. 建立有界共享 replay buffer，时间/条数/字节初始建议统一见决策清单 D-05，单连接队列与其他保护按 T-05 核定；BE-12 正式负载前冻结，不能把初始建议当作已验证容量。
6. 明确快照/订阅交接：先捕获 replay 边界并保证其后的提交进入缓冲，再读取 HTTP 快照，响应带该边界；订阅从边界补发。并发快照可能与补发重复，客户端按稳定记录 ID 去重。
7. 补齐 cursor 失效、buffer 溢出、进程重启/epoch 改变、提交通知缺口时返回 `resync_required` 等明确状态，由前端重新取 HTTP 快照；不能宣称无限回放或无损。
8. 心跳、空闲、连接关闭、服务 shutdown、登出、用户配置变更和 session 到期均有回收路径。握手通过不等于以后永久有效；须持续检查/接收会话撤销。
9. 关闭解析自动刷新取消对应订阅；若其他页面仍用连接，可保留连接但不得继续向旧订阅推记录。实时服务状态不依赖详情 enable。
10. 保留任务使 cursor 或记录过期时传播水位变更/重同步；客户端目录改变只影响后续显示快照，不推送“历史事实被修改”事件。

### 传输保护补充

当前 HTTP middleware 有总请求 deadline/并发许可。WS upgrade 后的长连接必须由独立有界连接 owner 接管，不能被普通 HTTP 15 秒处理超时误杀，也不能绕过总连接容量；握手校验和普通 API 限额继续有效。浏览器不可自行设置自定义 Origin 或暴露 token；认证不通过 URL query 传输。

### 验收

用真实 HTTP/WS 覆盖快照期间并发提交、迟到事件、去重、过滤变化、断线补齐、重启、新用户会话、Origin 伪造、缓冲/帧超限、慢消费者、心跳断开和 shutdown。有界 fixture 测试与真实连接测试分别记证据。

## 13. BE-11：新基线初始化与旧路径退出

2026-09-07 P0 源码核定：BC-26 完整生产初始化不能独立于 BC-04/07/08 至 BC-11 完成。当前 `StorageRuntime::open` 仍初始化统计/详情单库 v6，cache 仍装配可自动升级的 SQLite adapter；把 v2 字段接入这些 owner 会错误保留旧语义。P0 的 v2 拒绝规则/离线夹具随 BC-01 交付，不另造无消费者的空 layout/marker，也不将 BC-26 标为完成。完整空目录启动、重复启动、旧路径拒绝和新格式恢复待这些 owner 就绪后单独验证、提交；不删除个人运行数据。

按 D-10 删除原 BE-11 迁移预览、旧详情搬迁和维护回退任务，不顺延为未来必做项。

1. 提供新配置示例、schema 和全新统计/详情布局初始化，测试从空目录启动；SQL 初始化基础设施可复用，不继续累计旧版兼容转换。
2. 新库记录明确格式/layout 标识；旧配置或旧库路径误入时返回版本错误并指导使用新的开发目录，不猜测转换、不静默清空。
3. 清理正式调用链里的多 ID 旧配置映射、旧单库详情读写、名称统计维度和 legacy API 分支；先查询真实引用，保留仍被新逻辑复用的通用 parser/codec/SQL helper。
4. 同批更新示例、生成类型、fixture、CLI 错误和构建入口；不承诺新版数据能交回旧程序。
5. 新格式自身重启、双文件 journal、缓存损坏冷启、分片 manifest 和 ledger 恢复仍须测试；取消迁移不等于取消故障恢复。

### 验收

Windows 空目录初始化、正常写入/关闭/重启、误指旧格式拒绝、初始化中断和新格式恢复。只在明确临时目录操作测试数据，不删除个人 `_fluxdns/` 内容；无需旧库搬迁演练或 Linux 实机。

## 14. BE-12：验证、交付与文档收口

### 分层验证

| 层级 | 内容 | 不能替代的证据 |
| --- | --- | --- |
| 单元/契约 | 名称/身份矩阵、保留算法、cursor、活动源编辑、错误和字段白名单 | 不证明生产装配、真实磁盘或网络可用 |
| 真实 adapter | SQLite 分片/ledger、文件替换/journal、OS 采样、HTTP/WS | 不证明实际发布 binary 的端到端交付 |
| 集成 | 冷启/热更新/关闭、缓存/保留/推送交错、完整配置依赖链 | 与前端实际表单和浏览器安全联合核验 |
| 性能/故障 | Windows 约 10 客户端、预热缓存 2ms 测点、快照/清理并行、慢订阅者和空间错误 | 不以静态 fmt/typecheck、cache lookup 或无崩溃代替 |
| 基线/平台 | Windows 新格式冷启/重启、文件替换、DoH 模式；Linux 代码审查 | Linux 不要求专门运行，不把未测写成通过 |

沿用就近 `#[cfg(test)]` 和既有 contract tests，不新建重复测试平台。性能场景复用[后台服务验证入口](../implementation/backend/background-services.md)，按 D-11 在 release/预热缓存下测真实 core，不含客户端 I/O，报告样本数、P50/P95/P99、最大值和超过 2ms 样本。约 10 客户端场景覆盖快照/清理/热更新并行；高负载仅做有界队列/背压定向测试，不新增超高 QPS 或长时间 soak 门槛。

### 执行命令与边界

实施时先核对 [mise.toml](../../mise.toml) 和[环境规则](../rules/environment-usage.md)；后端从仓库根目录执行，前端生成类型从 `frontend/` 执行。以下为未来验证命令，不是本轮结果：

```powershell
cargo fmt --manifest-path backend/Cargo.toml -- --check
cargo test --manifest-path backend/Cargo.toml
```

```powershell
# 执行目录：frontend/
pnpm run generate:api
pnpm run typecheck
pnpm run test
pnpm run build
```

```powershell
# 执行目录：仓库根；内嵌验证前确保前端构建物存在
cargo test --manifest-path backend/Cargo.toml --features webui-embed
pwsh -File .agents/skills/project-doc-maintenance/scripts/check-docs.ps1
git diff --check
```

生产内嵌打包、启动和 smoke 使用[交付实现](../implementation/delivery.md)中的现有脚本与[本地测试规则](../rules/local-testing.md)，不在计划中虚构新 CLI 命令。DoH external 模式缺必要环境时记录未执行，不自行安装反向代理。

### 完成条件

- [ ] BE-01 至 BE-11 的目标接口与进程 owner 已进入正式启动、reload、shutdown 链路。
- [ ] 所有模块写入、外部差异/同步、身份/历史/快照/保留、HTTP/WS 和新基线有对应测试结果。
- [ ] 总计划 E2E-01 至 E2E-13 中后端责任已联合验收；未通过的平台/故障项明确保留。
- [ ] 正式配置参考/示例、OpenAPI/生成类型、Config/Policy/Cache/Storage/Management 架构和后端实现文档同批更新。
- [ ] 新路径正式接线后按真实引用删除被替代旧路径；未执行授权外的数据清理或发布。

按[总计划](webui-management-development-plan.md#7-交付控制与文档退出)沉淀实际事实后删除完成计划及索引；未完成验收不提前退出。

## 15. 后端阶段性提交检查点

每行默认是一个独立、可验证的 Git 提交边界，而不是等 BE-12 才统一提交。编号用于追踪，执行顺序以依赖为准；大任务可以继续拆成“内部能力 -> 正式接线”，但不提交无法编译或缺关联 schema/测试的半成品。跨端原子契约改动的最小生成类型/client/fixture 可以跟随后端提交，不必等整页 UI 完成。

验证级别使用[总计划提交规则](webui-management-development-plan.md#8-阶段性-git-提交)：`V-D` 文档，`V-B` 后端，`V-A` API，`V-I` 实际 adapter/集成。表中的验证均为该提交新增或变更范围的最低要求，不替代任务完整验收。

| 检查点 | 对应任务与本次提交范围 | 必要前置 | 最小验证 | 建议提交信息 |
| --- | --- | --- | --- | --- |
| BC-01 | BE-01：v2 契约、公共 DTO、schema/类型生成及新格式规则；不注册未实现写路由 | GC-01、实施启动 | V-D、V-B、V-A；所有受影响调用点可构建 | `refactor(management): 定义重构版本与接口契约` |
| BC-02 | BE-02：active_source、版本/操作状态、定向候选与引用编辑 | BC-01 | V-B；活动源与外部源隔离、名称引用 | `feat(config): 建立活动配置与候选编辑基础` |
| BC-03 | BE-02：服务控制命令、运行时应用、差量 listener 复用与补偿 | BC-02 | V-B、V-I；持续请求、未变端口复用及应用失败 | `feat(runtime): 支持配置热应用与差量监听切换` |
| BC-04 | BE-03：唯一 name 和单 client_id、ID/IP 索引及归一化 | BC-01 | V-B；重复名称/ID 拒绝、CIDR 矩阵 | `refactor(policy): 分离客户端管理键与请求标识` |
| BC-05 | BE-03：原始身份/历史匹配事件链路、详情模型、统计归属与缓存 fingerprint | BC-04 | V-B；transport 到 projector、reload 与隔离 | `feat(dns): 贯穿原始身份与历史匹配结果` |
| BC-06 | BE-04：有界内存导出、快照 codec、完整性与恢复校验 | BC-01 | V-B、V-I；TTL、预算、损坏文件 | `feat(cache): 实现有界二进制快照读写` |
| BC-07 | BE-04：进程级周期 worker、owner/generation、启动与关闭生产接线 | BC-06、BC-05 | V-B、V-I；reload/关闭交错、SQLite 缓存退出生产路径 | `refactor(cache): 切换为进程级周期快照` |
| BC-08 | BE-05：详情日分片 writer、layout、连接/lease 生命周期及无条数配额批写 | BC-05 | V-B、V-I；真实跨日插入、迟到和队列保护 | `refactor(storage): 按 UTC 日分片写入解析详情` |
| BC-09 | BE-05：稳定记录 ID、跨分片读取/cursor、提交后通知基础 | BC-08 | V-B、V-I；双向分页、同毫秒、过滤与通知时序 | `feat(storage): 增加跨日详情游标与提交读口` |
| BC-10 | BE-06：保留计算、共同水位、stats/详情读写保护及 manifest 回收 | BC-08、BC-09 | V-B、V-I；阈值等号、ledger 重放、退役禁止重建 | `feat(storage): 统一统计与详情保留水位` |
| BC-11 | BE-06：01:00 调度、补跑、预览/状态、物理回收失败重试 | BC-10 | V-B、V-I；时区、重启、删除失败和实际回收 | `feat(storage): 接入每日保留调度与回收状态` |
| BC-12 | BE-07：活动源/生效值、引用图、文件状态和系统白名单只读 API | BC-29、BC-30、BC-04 | V-B、V-A；只读/脱敏与版本 | `feat(management): 提供模块化配置查询` |
| BC-13 | BE-07：历史身份投影、查询过滤、跨日 API、大窗口和可查范围 | BC-09、BC-11、BC-12 | V-B、V-A、V-I；分页前过滤与新格式 | `feat(management): 提供新版解析历史查询` |
| BC-14 | BE-08C：代理配置读取/预校验/保存及引用处理 | BC-03、BC-12 | V-B、V-A；env/file 互斥、无秘密回显 | `feat(management): 支持代理配置编辑` |
| BC-15 | BE-08A：Hosts 资源受限编辑 | BC-03、BC-12 | V-B、V-A；格式/来源切换、首载失败 | `feat(management): 支持 Hosts 配置编辑` |
| BC-16 | BE-08B：规则集受限编辑 | BC-14、BC-12 | V-B、V-A；来源/格式、selector、proxy | `feat(management): 支持规则集配置编辑` |
| BC-17 | BE-08D：上游与组类型化保存、改名和引用关系 | BC-14、BC-15、BC-12 | V-B、V-A；旧键、组循环、权重及类型切换 | `feat(management): 支持上游及上游组编辑` |
| BC-18 | BE-08E：策略规则/覆盖保存 | BC-15、BC-16、BC-17 | V-B、V-A；规则顺序、引用、继承/禁用 | `feat(management): 支持 DNS 分流策略编辑` |
| BC-19 | BE-08F：listener/DoH 配置保存和重绑反馈 | BC-18、BC-03 | V-B、V-A、V-I；端口冲突及真实重绑 | `feat(management): 支持监听入口配置编辑` |
| BC-20 | BE-08G：客户端按 name 创建/编辑及目录更新 | BC-04、BC-05、BC-18 | V-B、V-A；name 唯一、ID 只读、CIDR 及历史不变 | `feat(management): 支持客户端配置编辑` |
| BC-21 | BE-08H：DNS 缓存/TTL/ECS、R/G/T 与详情热开关保存 | BC-03、BC-07、BC-11、BC-12 | V-B、V-A、V-I；预览确认和任务边界 | `feat(management): 支持 DNS 全局配置编辑` |
| BC-22 | BE-08I：日志受限热更新端点及系统只读拒绝 | BC-31、BC-12 | V-B、V-A；字段注入、热生效和未同步 | `feat(management): 支持日志配置热更新` |
| BC-23 | BE-09：请求窗口、在线身份、OS 采样与查询端点 | BC-01、D-04/D-08 | V-B、V-A、V-I；已知流量和真实进程采样 | `feat(management): 提供实时服务与进程指标` |
| BC-24 | BE-10：WS 鉴权/生命周期/限额、服务指标通道 | BC-23、D-05/D-08 | V-B、V-A、V-I；Origin、过期会话、长连接与慢消费者 | `feat(management): 接入服务指标实时推送` |
| BC-25 | BE-10：记录提交推送、replay、快照交接和 resync | BC-24、BC-09、BC-13 | V-B、V-A、V-I；并发提交/迟到/断线/溢出 | `feat(management): 接入解析记录增量推送` |
| BC-26 | BE-11：新配置/数据基线初始化、旧格式拒绝和开发夹具 | 离线规格可随 BC-01；完整生产初始化依赖 BC-04、BC-07、BC-08 至 BC-11（及各自前置） | V-B、V-I；空目录、新格式重复启动、旧路径拒绝 | `refactor(storage): 建立新版配置与数据初始化基线` |
| BC-27 | BE-11：旧配置映射、单库详情及 legacy 兼容路径退出 | BC-26、BC-07、BC-08 至 BC-13、BC-25；新路径已接线 | V-B、V-I、V-D；引用检查、新版重启/恢复 | `refactor(backend): 移除被替代的旧版兼容路径` |
| BC-28 | BE-12：Windows 组合、约 10 客户端和 2ms 核心耗时验收 | BC-01 至 BC-27、BC-29 至 BC-32 | V-B、V-I、V-D；真实集成和时延结果 | `test(backend): 补齐管理后台重构集成验收` |
| BC-29 | BE-02：应用后持久化、分阶段 journal、重试同步与恢复 | BC-03 | V-B、V-I；磁盘失败及双文件 crash point | `feat(config): 支持热应用后的配置持久化与恢复` |
| BC-30 | BE-02：仅提示 watcher、差异/还原/组合采用内部能力及状态端点 | BC-29 | V-B、V-A、V-I；外改不 reload、二次冲突、脱敏 | `feat(config): 增加外部配置变更处理流程` |
| BC-31 | BE-02：日志进程 owner、开关/级别/输出热切换 | BC-03、BC-29 | V-B、V-I；off/on、预开失败、shutdown | `feat(logging): 支持日志配置不停机切换` |
| BC-32 | BE-02：配置/文件/owner 跨模块故障回归 | BC-30、BC-31、BC-19、BC-21、BC-22 | V-B、V-I；组合采用、并发刷新、响应丢失 | `test(config): 补齐热更新与文件同步故障覆盖` |

每个提交同步其直接受影响的测试、正式契约、示例和实现说明；BC-28 只负责跨模块新增验证及其证据，不能承担补写所有前置提交测试的工作。重构期间内部能力尚未生产接线时，文档明确“未接线”，不提前启用新格式/路由或声称已生效。

BC-26/27 已替换原迁移工作包，不实施旧库搬迁。BC-29 至 BC-31 必须按依赖在模块写入前推进，编号不是数字串行顺序。所有代码提交不包含删除不明本地数据或 push。
