# WebUI 管理后台后端重构开发计划

> 文档状态：草案
>
> 计划状态：待评审
>
> 适用范围：管理后台配套 Rust 后端、配置和存储迁移、HTTP/WS 契约及后端验收
>
> 代码基线：`48832e47305e44733ef4ffdfd99db5007aef542e`（2026-09-07 定向静态核对，不含构建或运行验收）
>
> 总览：[开发总计划](webui-management-development-plan.md)
>
> 设计依据：[配套后端重构方案](webui-management-backend-refactor.md) · [需求与图稿解释](webui-management-requirements.md)

待决策项、推荐/备选方案、参数核定和确认结果统一见[决策清单](webui-management-decisions.md)。本计划保留实施要求与验收，涉及未确认选择时不能把下文目标行为当作已批准契约。

## 1. 范围与实现原则

本计划将原方案 B1-B8 展开为可交付任务，不替代原方案中的身份矩阵、快照协议、保留算法和迁移限制。所有任务待评审；本次不执行代码改造和数据迁移。

- 沿 `Config -> Runtime -> DNS/Policy -> ports -> adapters` 扩展；Management handler 不直接操作 SQLx pool、DNS 缓存集合或源 YAML。
- 请求路径只捕获 typed 事实、执行 DNS 与发布有界事件；快照、历史查询、名称关联、清理和磁盘写入全部在后台。
- 复用现有候选校验、CAS、进程 owner、Supervisor、ConfigStore journal、SQLx 和 batch ledger；不能再建立后台专属的第二套配置或存储权威。
- 配置源 revision、运行 revision、客户端目录 revision、缓存 owner/generation、存储 layout version 和 WS stream epoch 各司其职，不能混用。
- 新增字段按 schema 主版本实施；旧身份缺失不能靠当前配置补造。删除旧代码以新生产接线和迁移验收为前提。
- 依赖/feature 变更先完成总计划 D-08；不在本计划承诺安装新工具或替换框架。

## 2. 任务与源码边界

| 任务 | 内容 | 主要现有入口 | 依赖 |
| --- | --- | --- | --- |
| BE-01 | 版本、配置与 API 契约 | [model](../../backend/src/config/model.rs)、[resolve](../../backend/src/config/resolve.rs)、[validate](../../backend/src/config/validate.rs)、[migrate](../../backend/src/config/migrate.rs)、[OpenAPI](../../frontend/openapi/management-api-v1.yaml) | 总计划 D-01 至 D-07 |
| BE-02 | 配置候选写入与生效 | [store](../../backend/src/config/store.rs)、[source_edit](../../backend/src/config/source_edit.rs)、[coordinator](../../backend/src/runtime/coordinator.rs)、[Management server](../../backend/src/management/server.rs) | BE-01、D-02 |
| BE-03 | 客户端身份和匹配 | [DNS context](../../backend/src/dns/context.rs)、[client](../../backend/src/policy/client.rs)、[DNS Policy](../../backend/src/dns/policy.rs)、[observation](../../backend/src/ports/observation.rs)、[resolve_log](../../backend/src/storage/resolve_log.rs) | BE-01 |
| BE-04 | 独立缓存快照 | [cache service](../../backend/src/cache/service.rs)、[memory](../../backend/src/cache/memory.rs)、[moka](../../backend/src/cache/moka.rs)、[persistence](../../backend/src/cache/persistence.rs)、[cache runtime](../../backend/src/cache/runtime.rs) | BE-01；与 BE-03 共同核验 fingerprint |
| BE-05 | 详情日分片和提交读口 | [storage service](../../backend/src/storage/service.rs)、[sqlite](../../backend/src/storage/sqlite.rs)、[writer](../../backend/src/storage/writer.rs)、[management_read](../../backend/src/storage/management_read.rs)、[storage port](../../backend/src/ports/storage.rs) | BE-01、BE-03 |
| BE-06 | 统一保留协调器 | [stats](../../backend/src/storage/stats.rs)、[statistics](../../backend/src/storage/statistics.rs)、[ledger](../../backend/src/storage/ledger.rs)、storage service | BE-05、D-06/D-08 |
| BE-07 | 配置与观测查询 | [management port](../../backend/src/ports/management.rs)、[query](../../backend/src/management/query.rs)、management_read、[router](../../backend/src/management/router.rs) | 配置读依赖 BE-02；历史读依赖 BE-03/05/06 |
| BE-08 | 模块级受限写接口 | router、ConfigStore、source_edit、config validate/resolve | BE-02、BE-07 配置读；按模块接入 BE-03/04/06 |
| BE-09 | 实时指标与进程采样 | [DNS service](../../backend/src/service.rs)、[telemetry port](../../backend/src/ports/telemetry.rs)、query、management server | BE-01、D-04/D-08 |
| BE-10 | WebSocket 与补齐 | management router/server、management port、详情提交事件、指标采样 | BE-05、BE-07、BE-09、D-05/D-08 |
| BE-11 | 迁移与维护回退 | config migrate/load、[migrations](../../backend/migrations)、storage service、[app](../../backend/src/app.rs) | 预览从 BE-01 开始；实际转换依赖 BE-03/04/05/06 |
| BE-12 | 组合、安全和负载验收 | 各模块就近测试、[backend contract tests](../../backend/src/storage/backend_contract_tests.rs)、[交付入口](../implementation/delivery.md) | BE-01 至 BE-11、前端联合验收 |

表中链接为现有入口，不表示这些文件已实现目标能力。新增源码文件仅在已有模块内部按真实职责拆分，例如详情分片、保留或 WS adapter；不预建新的顶层架构层。

## 3. BE-01：冻结版本和类型契约

### 开发步骤

1. 形成总计划决策记录，确定配置/API 主版本及旧请求失败方式。配置版本、统计 DB schema、详情 layout、缓存格式和 WS 协议版本分别管理。
2. 更新配置 DTO/归一化/校验提案：单个 `client_id`、可选名称及 IP 匹配；快照 `enabled/path/snapshot_interval`；`statistics.retention`；`database.records_path`；详情配置仅 `enable`。
3. 明确源值与生效值、未设置与显式禁用的类型表示，沿当前配置校验处理三态，不能凭空把所有缺失改成 `false` 或 `null`。
4. 定义资源读取/预校验/保存的公共 envelope、错误路径、expected revision、旧键、应用方式及当前状态查询。业务 payload 使用各自严格类型，不接收任意 JSON Patch 或整份 YAML。
5. 定义身份 DTO、稳定记录 ID、历史分页 cursor、提交序列 cursor、保留预览/状态、指标单位与可用性。WS 消息体的 schema 与 HTTP DTO 共用唯一类型权威，不另手写一套前端事件模型。
6. 明确有效载荷、分页、天数、查询时间、WS 帧/速率/队列、在线身份数量等保护值及超限错误。详情条数业务配额删除不等于取消资源保护。
7. 同批更新正式 OpenAPI、类型生成脚本输入和生成类型、合法/非法 fixture；不手工编辑 `generated.ts`。API 切换同步考虑 auth client、Vite 开发代理、mock、SPA API fallback。

### 交付与验收

- 每个字段能追溯到需求、图稿或现有 schema；不存在虚构 listener 开关、额外上游协议、缓存持久化配额。
- 严格解析测试覆盖未知字段、类型分支残留、单位/溢出、非法 CIDR、路径别名/碰撞、缺失/禁用与引用环。
- 新旧版本 fixture 明确区分；旧配置无法无损转换时产生预览和阻塞项，不静默猜测。
- FE-01/FE-02 可使用生成类型开始开发，但不把 fixture 当作正式 handler 已就绪。

## 4. BE-02：配置事务、revision 与运行时应用

### 4.1 候选流水线

当前 `ConfigStore::create_initial_user` 只定向修改 users。新增面向资源的内部命令，按以下边界扩展：

```text
认证及同源校验
 -> 校验资源命令/expected source revision
 -> 读取并检查源文件 fingerprint
 -> 定向编辑源 AST + 更新批准的类型化引用
 -> strict load/validate/resolve
 -> prepare 与生效方式预检
 -> 再核对 source/runtime revision 并提交文件 journal
 -> 激活候选或登记待重启/待任务边界
 -> 返回保存事实、应用事实与新的 revision
```

1. 保留源 YAML 未改字段、SecretRef 引用和可支持的表达方式。不支持的源表示返回安全错误；禁止通过序列化 `ResolvedConfig` 覆盖源文件来兜底。
2. 序列化管理写入与外部 watcher 激活的提交边界。prepare 期间允许耗时工作，但最终必须再做 source/runtime CAS；不得持有 DNS 查询锁等待磁盘或网络。
3. 引用重写、资源替换和校验组成一个候选；任何引用失败都不能留下半次改名。
4. 复用 staged 文件、journal 和启动恢复，覆盖源与快照为同一路径/不同路径。多文件替换和运行激活不是跨介质原子事务，必须逐失败点定义状态。
5. 对内部配置写入扩展 fingerprint 识别，不因普通模块保存撤销所有 session；真正的用户配置变化保持现有 session 撤销语义。

### 4.2 失败与重试

| 失败点 | 持久化/运行结果 | 对外与恢复要求 |
| --- | --- | --- |
| 字段校验或 prepare 失败 | 源不变，活动 runtime 不变 | 返回字段错误及安全原因 |
| source/runtime CAS 冲突 | 本次不覆盖他人变更 | 冲突响应，需重新读取后由用户决定 |
| 提交文件失败或 journal 未完成 | 不能宣称保存完成 | 报告可恢复/维护状态；后续写入遵守事务恢复门槛 |
| 文件已保存、激活失败 | 源为新值，运行仍为旧值 | 明确“已保存、未应用”，携带各自 revision；不把它包装成纯失败让前端盲重试 |
| 需要重启 | 源为新值，进程仍用旧配置 | 返回待重启字段及生效模式；不自动重启 |
| HTTP 响应丢失 | 提交可能已发生 | 前端回读源 revision/资源及应用状态后判断，不自动重放创建/改名 |

日志先保存为待重启后，再编辑可热应用资源是必测场景。按[决策清单 D-02](webui-management-decisions.md#d-02-保存应用与待重启配置)选择应用方式；采用独立热应用时，候选预检分离“已保存源配置”和“本进程可应用投影”，保留旧进程级字段并应用本次允许的热变更。必须复用 Runtime 的 restart-required 分类，不能把需要重启的字段偷偷生效；无法可靠分离时报告并重新确认。

### 4.3 生效方式

| 变更 | 目标行为 |
| --- | --- |
| 客户端名称 | 发布新目录，不重写历史、不要求重启；不因纯显示修改改变缓存语义 |
| 客户端 IP/策略/覆盖、全局 TTL/ECS、策略/资源/上游 | 完整候选 prepare 后应用，保留旧实例 drain 和 fingerprint 约束 |
| listener 地址、协议、DoH routes/endpoints | 走现有 bind/preflight/activate 能力；不能安全重绑时拒绝或明确需维护，不先停旧入口再无条件保存 |
| 内存预算 | 更新共享预算并按准入/淘汰规则生效，不重置所有持久化/统计 owner |
| 快照开关、路径、周期 | 进程 owner 串行切换；间隔在任务边界更新，旧 owner 无发布权；路径变化不重新预热已运行缓存 |
| R/G/T | 保存后下次保留任务冻结新 revision；不在 HTTP 保存中直接删除历史 |
| 详情 enable | 目标为受控热应用；关闭停止接纳新详情并有界处理已入队数据，统计继续运行 |
| logs | 仅持久化允许字段，返回需要重启 |
| database/webui/work | 普通管理 API 不接受写入 |

### 验收

测试并发保存、外部文件修改、相同内容重复提交、响应丢失、prepare 超时、磁盘满、权限失败、每个 journal crash point、应用 CAS 失败、待重启后继续编辑及重启恢复。敏感内容不进入错误、Debug、审计摘要；至少验证一类资源真实热应用和日志待重启。

## 5. BE-03：客户端身份贯穿请求与投影

### 开发步骤

1. 在 [UDP](../../backend/src/transport/udp.rs)、[TCP](../../backend/src/transport/tcp.rs)、[DoH](../../backend/src/transport/doh.rs) 到 `RequestContext` 的路径核对原始 ID/IP 捕获。无 ID 协议保持 `null`；DoH 仅接受现有可信来源的有效 IP。
2. 将单 ID 主键接入 resolved client、索引和配置引用，保留 ID 优先与最长 CIDR 回退；IPv4-mapped IPv6 归一化保持一致。
3. Policy 在当前请求 runtime 中冻结 `matched_client_id` 与来源，不增加第二次匹配；经完成事件和 detail projector 写入原始身份与最小匹配结果。
4. 区分 `identity_status` 与旧 `detail_status`：原始身份未记录、Answer 已脱敏、无 ID、未匹配不是同一状态。
5. 新统计按匹配 ID 或有界 unknown 维度归属；事件消费/reload 不重映射。旧名称维度带 legacy 版本，不与新 ID 同名字符串混算。
6. 缓存保留 ID 命中按实际 ID、IP 命中按实际 IP 的域分隔隔离，结合生效策略；同 CIDR 的不同 IP 不能合并为同一个客户端池。
7. 控制新增 ID 字段的长度、序列化和日志边界；请求原始 ID/IP 不得变成无界 telemetry labels。

### 验收

以原方案的[请求期匹配矩阵](webui-management-backend-refactor.md#43-请求期匹配矩阵)为测试数据表，覆盖 ID/IP 冲突、未知 ID 回退、同名不同 ID、CIDR 包含/重复、删除后复用、reload 与异步消费交错。校验存储列真实值、统计归属和缓存隔离，不只检查 API 文案。新 UDP 记录与 legacy 记录必须可区分。

交付 BE-05 的详情模型、BE-07 的查询投影及 FE-04/FE-10 的 typed fixture。

## 6. BE-04：内存权威与独立缓存快照

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

真实临时目录测试开关、文件缺失、损坏、未知版本、权限/空间失败、恢复超时、预算缩小、停机 TTL、并发淘汰、clear/reload/shutdown 交错及 Windows/Linux 替换。记录导出锁占用、RSS 和旧文件加临时文件的 I/O 峰值，不把内存预算宣称为磁盘硬配额。

生产启动路径不再创建缓存 SQLite；统计库/详情文件保持不变；FE-07 能显示成功、部分恢复、冷启与失败原因。

## 7. BE-05：UTC 日分片详情存储

### 开发步骤

1. 定义详情 layout version、日期路径解析、受管理分片目录及索引。新库路径与统计库/缓存路径严格分离，文件名只来自已解析 UTC 日期。
2. 拆分业务统计 backend 与 detail writer 的所有权，保持现有有界事件/详情队列和非阻塞发布。详情仅按事件日写入，禁止每个历史日常驻连接。
3. 批写事务只做有界校验与 INSERT，不再调用历史 COUNT、按条数淘汰、历史 DELETE/VACUUM；业务条数上限删除后仍有批次/队列/响应保护。
4. 增加分片 registry 和读写 lease、受限活动连接与关闭流程；为 BE-06 提供禁止新写入、排空、checkpoint/关闭、退役和恢复入口。
5. 定义含分片定位信息的不透明稳定记录 ID，排序为事件 UTC 毫秒加稳定 ID；keyset cursor 带过滤/排序上下文，不把单库自增 ID 当全局 ID。
6. 按时间范围定位日期集合，在各分片先应用过滤和索引，再合并有界结果。总数单独按相同过滤聚合且有超时；不能使用“当前页长度”冒充总数。
7. 跨页支持前后 cursor；保留图稿的分页交互目标，但任意页码直跳按 D-05 评审，不通过对巨大历史执行无界 OFFSET 勉强兼容。
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
| 各配置模块 | 源配置字段、类型、引用选项/数量、revision、继承来源、生效/待应用摘要 | ConfigStore 源视图 + 当前 runtime/资源快照，按同次读取边界标识 |
| 监听入口 | 逻辑入口、多地址绑定、DoH routes/endpoints、真实绑定状态和 ECS 来源 | 配置及 runtime bind 信息；不存在独立 enable 时不伪造 |
| DNS 上游/组 | 当前配置类型、连接/成员/模式/回退、被引用位置 | 同一配置 revision 的类型化引用图 |
| DNS 配置 | 全局缓存/TTL/ECS、R/G/T、详情开关及快照/保留状态 | BE-04、BE-06 的只读状态 |
| Hosts/规则集 | 源类型、格式、内容或来源、更新计划与有效/陈旧状态 | 源配置与 resource metadata，内容只读取批准的内联配置 |
| 客户端 | 唯一 ID、当前名称、IP/CIDR、策略/覆盖与目录 revision | 单次当前客户端目录快照 |
| 代理/系统配置 | 批准的 SecretRef 来源与系统源路径、logs、只读 database/webui/work | D-07 白名单投影，禁止 Secret 实际值与 users/hash |
| 解析记录 | 全部原始/历史/当前显示信息，Answer/provenance、过滤、详情定位和跨日分页 | BE-05 分片读口 + BE-06 水位 + 当前目录 |
| 服务/进程信息 | 采样时间、单位、窗口、可用性及数值 | BE-09 共享进程采样器和请求观测 |

### 9.2 查询实施要点

1. 扩展 `ManagementStorageRead` 和领域结果，不把 SQLx/HTTP 类型引入核心 port；全部过滤采用固定模板/绑定参数。
2. 原始 ID/IP、匹配 ID、域名、协议、来源、响应码、结果状态、时间和排序在分页前过滤。名称搜索先解析当前 ID 集合，再过滤历史匹配 ID；同名不能只取一个。
3. 一页关联一次当前目录，不逐行查询。原始值与匹配结果保持历史事实，当前名称、配置是否存在及 directory revision 单独返回。
4. 保存旧 `detail_status`、有界 Answer、truncated 数量、耗时缺失、实际/目标上游及缓存 producer provenance，不能为新列表丢掉旧已支持语义。
5. 默认 R 天，允许显式访问尚可查宽限日；大于 31 天窗口用分段聚合/有界扫描和明确超限结果，删除旧硬编码限制而非默默截断。
6. 区分 unavailable、空结果、记录不存在、已超出水位、cursor 过期和 deadline。保留清理与查询交错时检查水位版本，必要时要求重取快照，不返回失效 cursor 伪装成最后一页。
7. 配置 GET 不等于文件浏览：只返回 D-07 批准的源字段，不打开任意传入路径，不回显进程解析后的绝对路径、秘密内容或全量 resolved config。

### 验收

HTTP 契约对照实际 JSON 与 OpenAPI；单次名称快照、一对多名称筛选、改名/删除/ID 复用、分页前筛选、legacy、日期水位和大窗口测试通过。未授权/错误响应和日志中不能出现配置秘密或查询原文泄漏。

## 10. BE-08：逐模块写接口

### 10.1 通用要求

每组提供读取、预校验及保存能力。所有写入使用 BE-02 事务、严格 DTO、expected revision 和稳定旧键，拒绝请求中混入其他模块字段。正式端点/字段在 BE-01 写入 OpenAPI，本表不另建完整 API schema。

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
| BE-08G 客户端 | 新建唯一 ID、编辑名称/IP/覆盖；普通编辑不接受 ID 改写 | BE-03 + 策略 | 同名不同 ID、空 IP、规范化 CIDR 冲突、原始无 ID 的后续请求 |
| BE-08H DNS 配置 | 缓存、TTL/ECS、statistics retention、详情 enable 分区受限保存；R/G/T 一次提交 | BE-04/06 + 热开关 owner | 预算/周期单位、缩短预览确认、任务边界 revision、详情关闭统计不停止 |
| BE-08I logs | 仅 enable/level/path；其余系统字段严格只读 | BE-02 重启状态 | 注入 database/work/webui 被拒；保存日志后运行值仍旧 |

上游及组改名按类型遍历所有引用，包括其他 DoH bootstrap、组主/回退成员、策略默认/规则上游；相关资源改名同时处理 listener hosts/策略、客户端策略、rule_set selector 的资源部分和 proxy 引用。禁止对 YAML 原文全局字符串替换，不能把 `geosite:cn` 的 selector 部分或自由文本误改。

组模式延续既有规则：parallel/failover 不接受可编辑权重，round-robin/load-balance 才使用权重；DNS 终态响应不能一概当作触发 fallback 的传输失败。配置编辑不修改核心算法。

### 验收与交接

每个子任务必须提供正常保存、字段错误、revision 冲突、引用失败、prepare/应用失败及保存后查询回显测试。FE 对应模块只有在真实保存与生效/重启状态联调后才能完成；mock 编辑成功不能关闭 BE-08 子任务。

## 11. BE-09：服务指标和进程信息

### 口径确认入口

QPS/RPM 计数、窗口、趋势粒度、在线身份去重、暖机与内存单位统一见[决策清单 D-04](webui-management-decisions.md#d-04-指标口径与在线客户端)。采样方案与容量核定分别见 D-08、T-05；确认后纳入正式契约，不在本节维护另一份参数建议表。

### 开发步骤

1. 选择接入计数边界并防止 TCP 多请求、重试、single-flight 和迟到上游重复计数；实现有界秒/分钟桶。
2. 在线身份从已验证请求上下文捕获，与详情开关解耦。仅保留满足 60 秒窗口的有界脱敏 key，不将原始 ID/IP 写入统计库或 telemetry label。
3. 在线身份容量达到上限时显式返回不完整/不可用原因，不能把截断后的数量宣称精确；已覆盖完整窗口但没有请求才可报告真实 0。
4. 进程启动不足 60/600 秒返回覆盖时长和 warming 状态，正式完整窗口数值未就绪时不把停机区间填零。采样失败与观测丢失返回缺口标记。
5. 进程采样器单 owner 定期更新，各页面/WS 读取共享快照；不得每个订阅连接启动独立 OS 采样。
6. 校验 Windows/Linux 进程信息及时区依赖，采样错误局部降级。服务状态不因此虚假显示健康，DNS 请求不等待采样。

### 验收

受控时钟与已知请求序列核算两个窗口、趋势桶和在线身份；覆盖 NAT、同 IP 不同 ID、unknown ID、详情关闭、窗口暖机、观测缺口、进程采样失败及基数上限。真实进程数据采样至少覆盖目标平台，内存口径与单位保持一致。

## 12. BE-10：WebSocket、序列与断线补齐

### 开发步骤

1. 在独立 Management Axum adapter 上启用经批准的 WS 能力；保持同源 Cookie session、Origin 校验与受监督生命周期，不复用 DoH parser。
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

## 13. BE-11：配置预览、存储迁移与回退

### 13.1 先开发只读预览

1. 定义离线预览输入/输出、版本、需要用户选择的映射及确认依据；预览不写源配置或业务库，不依赖网络和 Secret 实际值。
2. 单旧 ID 可建议直接转换；多 ID 拆分需选择 IP/CIDR 所属；纯 IP 客户端需用户分配唯一 ID；名称保留为显示值。
3. 废弃缓存容量与详情条数设置逐项报告，不能把它们换算成 T/R；旧 max age 只提供明确的天数取整建议，R/G/T 仍需确认。
4. 清点现有 schema、UTC 日期/条数、legacy 详情与统计、WAL/可用空间和冲突目标路径；缓存旧 SQLite 不强制导入。

### 13.2 经授权的执行步骤

1. 获取维护窗口、停写、备份路径、迁移和后续清理授权；对备份进行恢复可读性验证。测试只操作 `_fluxdns/` 下的明确夹具。
2. 停旧 writer，取得业务 SQLite 一致状态及配置副本，记录版本与摘要；不能只复制正在写入的主文件而忽略 WAL。
3. 分批读取旧详情，按事件 UTC 日写入新的目标分片；保留缺失标记，不从 `client_bucket` 或当前目录补原始身份/历史匹配。
4. 每批或每分片记录可恢复进度；重复运行不重复插入，失败不覆盖原库。迁移期间不按新保留期限丢弃历史。
5. 校验总条数、每日条数、日期边界、关键字段摘要、稳定记录映射、索引和查询结果；旧统计用版本化 legacy 维度读取。
6. 成功后提交 layout/manifest 并切换 reader/writer；启动只读 Management pool 在迁移成功之后。首次清理按已确认策略，不能在校验前提前回收。
7. 原详情表、旧缓存、备份与 WAL 整理属于单独明确维护步骤，不能自动 VACUUM 或以通配符批量删除。

### 13.3 回退

使用旧程序、旧配置与切换前一致备份，先停新 writer 再恢复。明确切换后新增记录不一定能交回旧程序，已清理历史不能凭代码回退恢复；缓存允许冷启，不成为业务回退依赖。

### 验收

空库、当前 schema v6 夹具、正常多日库、含 legacy 数据、异常日期/磁盘空间、复制中断、切换中断、重复执行和维护回退都应有真实 SQLite 演练。普通启动不得静默确认歧义/有损迁移或自动清库。

## 14. BE-12：验证、交付与文档收口

### 分层验证

| 层级 | 内容 | 不能替代的证据 |
| --- | --- | --- |
| 单元/契约 | 迁移映射、身份矩阵、保留算法、cursor、source edit、错误和字段白名单 | 不证明生产装配、真实磁盘或网络可用 |
| 真实 adapter | SQLite 分片/ledger、文件替换/journal、OS 采样、HTTP/WS | 不证明实际发布 binary 的端到端交付 |
| 集成 | 冷启/热更新/关闭、缓存/保留/推送交错、完整配置依赖链 | 与前端实际表单和浏览器安全联合核验 |
| 性能/故障 | 同机基线比较、目标/超载流量、快照 I/O、01:00 清理、慢订阅者、空间错误 | 不以静态 fmt/typecheck 或无崩溃代替 |
| 迁移/平台 | Windows/Linux、旧库恢复、维护回退、DoH 模式 | 缺环境的项保留待验收，不能标为通过 |

沿用就近 `#[cfg(test)]` 和已有 backend contract tests，不预设不存在的 `backend/tests/` 工程结构。性能场景复用[后台服务验证入口](../implementation/backend/background-services.md)，报告吞吐、P50/P95/P99、RSS、锁等待、连接上限、队列丢弃、快照大小/耗时和清理耗时；先固定接受阈值再运行。

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
- [ ] 所有模块写入、身份/历史/快照/保留、HTTP/WS 和迁移场景有对应测试结果。
- [ ] 总计划 E2E-01 至 E2E-13 中后端责任已联合验收；未通过的平台/故障项明确保留。
- [ ] 正式配置参考/示例、OpenAPI/生成类型、Config/Policy/Cache/Storage/Management 架构和后端实现文档同批更新。
- [ ] 被替换旧路径在迁移验收后按实际引用删除，未执行授权外的数据清理或发布。

按[总计划](webui-management-development-plan.md#7-交付控制与文档退出)沉淀实际事实后删除完成计划及索引；未完成验收不提前退出。

## 15. 后端阶段性提交检查点

每行默认是一个独立、可验证的 Git 提交边界，而不是等 BE-12 才统一提交。编号用于追踪，执行顺序以依赖为准；大任务可以继续拆成“内部能力 -> 正式接线”，但不提交无法编译或缺关联 schema/测试的半成品。跨端原子契约改动的最小生成类型/client/fixture 可以跟随后端提交，不必等整页 UI 完成。

验证级别使用[总计划提交规则](webui-management-development-plan.md#8-阶段性-git-提交)：`V-D` 文档，`V-B` 后端，`V-A` API，`V-I` 实际 adapter/集成。表中的验证均为该提交新增或变更范围的最低要求，不替代任务完整验收。

| 检查点 | 对应任务与本次提交范围 | 必要前置 | 最小验证 | 建议提交信息 |
| --- | --- | --- | --- | --- |
| BC-01 | BE-01：版本化契约、公共 DTO、schema/类型生成及迁移规则；不注册未实现写路由 | GC-01 及相关决策获批 | V-D、V-B、V-A；旧调用点仍可构建 | `refactor(management): 定义重构版本与接口契约` |
| BC-02 | BE-02：定向 source edit、候选提交、revision/journal 扩展；暂不开放模块写端点 | BC-01 | V-B、V-I；源冲突及文件失败恢复 | `feat(config): 增加资源级候选配置事务` |
| BC-03 | BE-02：运行时应用、待重启状态和 watcher 协调接线 | BC-02 | V-B、V-I；保存后应用失败、日志待重启后热修改 | `feat(runtime): 接入配置应用与生效状态` |
| BC-04 | BE-03：单客户端 ID 模型、ID/IP 索引、归一化及相关配置迁移测试 | BC-01 | V-B；ID/CIDR 矩阵、同名及旧配置歧义 | `refactor(policy): 使用单客户端标识维护匹配索引` |
| BC-05 | BE-03：原始身份/历史匹配事件链路、详情模型、统计归属与缓存 fingerprint | BC-04 | V-B；transport 到 projector、reload 与隔离 | `feat(dns): 贯穿原始身份与历史匹配结果` |
| BC-06 | BE-04：有界内存导出、快照 codec、完整性与恢复校验 | BC-01 | V-B、V-I；TTL、预算、损坏文件 | `feat(cache): 实现有界二进制快照读写` |
| BC-07 | BE-04：进程级周期 worker、owner/generation、启动与关闭生产接线 | BC-06、BC-05 | V-B、V-I；reload/关闭交错、SQLite 缓存退出生产路径 | `refactor(cache): 切换为进程级周期快照` |
| BC-08 | BE-05：详情日分片 writer、layout、连接/lease 生命周期及无条数配额批写 | BC-05 | V-B、V-I；真实跨日插入、迟到和队列保护 | `refactor(storage): 按 UTC 日分片写入解析详情` |
| BC-09 | BE-05：稳定记录 ID、跨分片读取/cursor、提交后通知基础 | BC-08 | V-B、V-I；双向分页、同毫秒、过滤与通知时序 | `feat(storage): 增加跨日详情游标与提交读口` |
| BC-10 | BE-06：保留计算、共同水位、stats/详情读写保护及 manifest 回收 | BC-08、BC-09 | V-B、V-I；阈值等号、ledger 重放、退役禁止重建 | `feat(storage): 统一统计与详情保留水位` |
| BC-11 | BE-06：01:00 调度、补跑、预览/状态、物理回收失败重试 | BC-10 | V-B、V-I；时区、重启、删除失败和实际回收 | `feat(storage): 接入每日保留调度与回收状态` |
| BC-12 | BE-07：配置源值/生效值、引用图、系统白名单只读 API | BC-03、BC-04、D-07 | V-B、V-A；只读/脱敏与目录 revision | `feat(management): 提供模块化配置查询` |
| BC-13 | BE-07：历史身份投影、查询过滤、跨日 API、大窗口和可查范围 | BC-09、BC-11、BC-12 | V-B、V-A、V-I；分页前过滤与 legacy | `feat(management): 提供新版解析历史查询` |
| BC-14 | BE-08C：代理配置读取/预校验/保存及引用处理 | BC-03、BC-12 | V-B、V-A；env/file 互斥、无秘密回显 | `feat(management): 支持代理配置编辑` |
| BC-15 | BE-08A：Hosts 资源受限编辑 | BC-03、BC-12 | V-B、V-A；格式/来源切换、首载失败 | `feat(management): 支持 Hosts 配置编辑` |
| BC-16 | BE-08B：规则集受限编辑 | BC-14、BC-12 | V-B、V-A；来源/格式、selector、proxy | `feat(management): 支持规则集配置编辑` |
| BC-17 | BE-08D：上游与组类型化保存、改名和引用关系 | BC-14、BC-15、BC-12 | V-B、V-A；旧键、组循环、权重及类型切换 | `feat(management): 支持上游及上游组编辑` |
| BC-18 | BE-08E：策略规则/覆盖保存 | BC-15、BC-16、BC-17 | V-B、V-A；规则顺序、引用、继承/禁用 | `feat(management): 支持 DNS 分流策略编辑` |
| BC-19 | BE-08F：listener/DoH 配置保存和重绑反馈 | BC-18、BC-03 | V-B、V-A、V-I；端口冲突及真实重绑 | `feat(management): 支持监听入口配置编辑` |
| BC-20 | BE-08G：客户端创建/编辑及目录更新 | BC-04、BC-05、BC-18 | V-B、V-A；ID 只读、同名、CIDR 及历史不变 | `feat(management): 支持客户端配置编辑` |
| BC-21 | BE-08H：DNS 缓存/TTL/ECS、R/G/T 与详情热开关保存 | BC-03、BC-07、BC-11、BC-12 | V-B、V-A、V-I；预览确认和任务边界 | `feat(management): 支持 DNS 全局配置编辑` |
| BC-22 | BE-08I：日志受限保存及系统只读拒绝 | BC-03、BC-12 | V-B、V-A；字段注入与重启状态 | `feat(management): 支持日志配置受限编辑` |
| BC-23 | BE-09：请求窗口、在线身份、OS 采样与查询端点 | BC-01、D-04/D-08 | V-B、V-A、V-I；已知流量和真实进程采样 | `feat(management): 提供实时服务与进程指标` |
| BC-24 | BE-10：WS 鉴权/生命周期/限额、服务指标通道 | BC-23、D-05/D-08 | V-B、V-A、V-I；Origin、过期会话、长连接与慢消费者 | `feat(management): 接入服务指标实时推送` |
| BC-25 | BE-10：记录提交推送、replay、快照交接和 resync | BC-24、BC-09、BC-13 | V-B、V-A、V-I；并发提交/迟到/断线/溢出 | `feat(management): 接入解析记录增量推送` |
| BC-26 | BE-11：离线迁移预览、旧配置歧义/存储清点及夹具 | BC-01；可提前于其他实现分支 | V-B、V-I；预览无写入、歧义必报 | `feat(migration): 增加重构迁移预览` |
| BC-27 | BE-11：业务库流式迁移、切换/恢复、维护回退流程 | BC-26、BC-08 至 BC-11；相关数据契约冻结 | V-B、V-I、V-D；真实旧库、中断和重复执行 | `feat(migration): 实现详情分片迁移与恢复` |
| BC-28 | BE-12：后端组合/故障回归用例和已完成验证的实现说明 | 上述后端检查点完成 | V-B、V-I、V-D；完整相关测试与负载证据 | `test(backend): 补齐管理后台重构集成验收` |

每个提交同步其直接受影响的测试、正式契约、示例和实现说明；BC-28 只负责跨模块新增验证及其证据，不能承担补写所有前置提交测试的工作。重构期间内部能力尚未生产接线时，文档明确“未接线”，不提前启用新格式/路由或声称已生效。

BC-27 的代码提交不授权对真实用户库执行迁移。代码级 revert 也不能代替业务回退，维护条件仍按 BE-11 执行。
