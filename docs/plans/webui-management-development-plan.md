# WebUI 管理后台重构开发总计划

> 文档状态：有效
>
> 计划状态：实施中
>
> 适用范围：WebUI 前后端重构的范围、契约决策、任务依赖、开发顺序、联合验收和交付收口
>
> 代码基线：`21fd23f3f711e2f7acc712b9ff715915c5248180`（2026-09-07 根据用户决策重订；定向核对配置、运行时、日志和管理入口）
>
> 上位依据：[管理后台需求](webui-management-requirements.md) · [配套后端重构方案](webui-management-backend-refactor.md) · [29 张设计图](webui-management-designs/README.md)

## 1. 结论与文档分工

采用“新契约先行、热更新底座、业务模块闭环、实时联调、Windows 验收”的顺序。先运行时应用再落盘，外部改文件只提示；不做旧数据迁移或旧 WebUI 兼容。前端壳层与固定契约下的表单可以先行，但不能仅靠 fixture 判定模块完成。

| 文档 | 唯一负责内容 |
| --- | --- |
| 本文 | 范围、决策依赖、全局依赖、阶段门槛与联合交付 |
| [决策清单](webui-management-decisions.md) | D-01 至 D-12 已确认结果、实施解释，以及技术核定和执行授权 |
| [配置热更新专项](webui-management-config-runtime-plan.md) | 活动配置权威、应用后持久化、外部差异处理、热 owner 与失败恢复；BE-02/FE-02 共用 |
| [后端开发计划](webui-management-backend-development-plan.md) | `BE-01` 至 `BE-12`：契约、配置事务、身份、缓存、存储、查询、模块写入、指标、推送、新基线和验证 |
| [前端开发计划](webui-management-frontend-development-plan.md) | `FE-01` 至 `FE-12`：导航、状态、表单、12 个模块、29 张图稿映射及浏览器验收 |
| [原需求](webui-management-requirements.md)与[原后端方案](webui-management-backend-refactor.md) | 产品范围、图稿解释，以及身份/缓存/保留的不变量；开发计划不复制全部设计正文 |

2026-09-07 用户在 P0 交付后追加授权实施 P1、必要验证和阶段性本地提交，不 push、不自动进入 P2。P1 实际开工基线为 `main` / `99f8ca7c97aebc403d75c4886668688441dfa47a`，工作树干净；本地 `origin/main` 同指此提交，未 fetch，不据此推断远端实时状态或推送者。GC-01 与两笔 BC-01 的祖先关系已核对，不重复实施。BC-26 的生产初始化依赖新 owner，详细剩余依赖见[BE-11](webui-management-backend-development-plan.md#13-be-11新基线初始化与旧路径退出)。

P1 先交付 BC-02 活动源、定向编辑和操作仲裁内部能力，事实见[配置参考](../implementation/configuration.md#p1-活动源与候选内部底座2026-09-07)；BC-03 已接入现有 service 的差量 socket、任务预注册、请求 drain 和有界队列消费者，完整应用事务仍未完成。BC-29 的[文件事务与分阶段恢复内部能力](../implementation/configuration.md#p1-应用后持久化内部底座2026-09-07)已由活动源消费，但 Runtime 回报仍为测试模拟，正式启动和 HTTP 未接线。v2 生产切换最小闭合集合仍是：新版 loader/resolve 与存储 owner 初始化、setup/auth/Origin、实际 handler、API client/代理/mock、SPA fallback 同批接线；不能将新字段映射到旧单库和 SQLite cache owner。BC-03/29/30/31 可以继续内部实施，但在 BC-26 依赖闭合前，不以内部测试关闭完整生产验收或仅修改版本常量。FC-16 只在本阶段交付可闭合的全局提示/还原基础，完整组合采用仍依赖各业务表单。

2026-09-07 继续执行时，BC-30 的[仅提示 watcher](../implementation/backend/lifecycle.md#p1-仅提示文件观测2026-09-07)已接入正式 app，双文件变更不触发 reload，Hosts 资源自动刷新有真实 UDP 定向证据。完整 P1 已设为执行目标，但原“不进入 P2”授权不变：完整 v2 初始化需要 BC-06/07 快照 owner、BC-08/09 日分片写入/读口、BC-10/11 共同保留水位/调度，再由 BC-26 初始化；其中 BC-04/05 身份链仍属 P1。是否将这些生产闭合必需的 P2 子项纳入本次执行，必须由用户另行决定。在决定前不实施这些子项、不关闭 P1，不扩展 BC-12/13 完整查询、业务页面、BC-24/25 WS、BC-27 总体旧路径退出或 P5 验收。其余 P1 内部能力仍有工作可做，该依赖不等于它们已完成。

P0 已落实 BC-01 配置、HTTP/WS、生成类型与路由/表单契约；BE-01 的生产 fixture 启动门槛随 BC-26 继续保留。实际能力、未接线边界和验证分别见[配置参考](../implementation/configuration.md#p0-v2-内部契约2026-09-07)、[Management 实现](../implementation/backend/management.md#p0-v2-契约)、[前端实现](../implementation/frontend/application.md#能力与证据)。本文不预设人员数量、固定人日或日历上线日期；排期以依赖和验收门槛为准。

2026-09-08 认证子项已按用户追加决定改为业务 Bearer、认证专用 Cookie 刷新，真实 HTTP 与浏览器回归见[Management 实现](../implementation/backend/management.md#p1-bearer-业务鉴权2026-09-08)。FC-01 又完成[12 路由壳层、浅色主题和响应式导航](../implementation/frontend/application.md#p1-应用壳层2026-09-08)；dashboard/queries 读取当前 v1 数据，FC-14 已提前接入[系统运行状态](../implementation/frontend/application.md#p1-系统运行状态2026-09-08)，其余入口保持明确空态或 tab 壳层。BC-04/05 已完成[客户端 name/ID 索引](../implementation/configuration.md#p1-客户端匹配索引内部能力2026-09-08)和[请求身份/历史匹配事件链](../implementation/backend/background-services.md#完成事件与后台分发)，生产新 loader 接线仍未完成。BC-23 已完成[服务指标、在线身份和共享 OS 采样](../implementation/backend/management.md#p1-服务与进程指标2026-09-08)。FC-02 已完成[配置交互公共基础](../implementation/frontend/application.md#p1-配置交互基础2026-09-08)，FC-16 已完成[提示/还原与同步重试基础](../implementation/frontend/application.md#p1-外部配置变化基础2026-09-08)，但不把未挂载组件或 MSW 当生产 v2 配置接口；v2 配置成套切换、FC-16 全局/组合接线和其他业务页面仍待后续依赖，P1 继续保持部分完成。

## 2. 当前基线与改造范围

### 2.1 已核对的工程入口

| 领域 | 当前事实与证据 | 计划影响 |
| --- | --- | --- |
| 前端 | [App](../../frontend/src/app/App.tsx) 和 [AppLayout](../../frontend/src/shared/components/AppLayout.tsx) 已注册 12 个目标入口；只有 dashboard/queries 接当前 v1 数据，其余为空态 | 继续接入 v2 与业务模块，保留认证边界，不并存两套正式后台 |
| 前端基础 | [package.json](../../frontend/package.json) 已有 React、TypeScript、Vite、Ant Design、TanStack Query、Router、Vitest/MSW | 复用工程和状态分层，不借重构更换整套技术栈 |
| API | [router](../../backend/src/management/router.rs)、[query](../../backend/src/management/query.rs) 和 [OpenAPI](../../frontend/openapi/management-api-v1.yaml) 为认证与只读查询；统计查询限制 31 天 | 增加受限配置读写、身份过滤、跨日查询、实时指标和 WebSocket |
| 配置写入 | [ConfigStore](../../backend/src/config/store.rs) 只有首用户定向写入、fingerprint/journal 与恢复 | 保存活动源表达，重建“先应用后持久化”事务及恢复门槛 |
| 文件与日志 | [app](../../backend/src/app.rs) watcher 已只提示；日志 owner 已复用现有 filter/输出并保持 writer，支持 service 热切换 | 继续闭合 v2 事务/持久化与 HTTP/UI；Windows 子项证据见[日志热切换](../implementation/backend/background-services.md#p1-日志热切换2026-09-07) |
| 客户端与详情 | [model](../../backend/src/config/model.rs)、[Policy](../../backend/src/policy/client.rs)、[observation](../../backend/src/ports/observation.rs) 使用名称、多 ID 匹配与 `client_bucket`；详情来源无原始 ID 字段 | 建立单 ID 主键及原始身份、当时匹配、当前显示信息三层语义 |
| 缓存与历史 | [缓存装配](../../backend/src/dns/policy.rs) 使用 SQLite persistence；[详情批写](../../backend/src/storage/sqlite.rs) 含历史清理和 COUNT | 切换独立快照、详情日分片和统一保留协调器 |
| 生命周期 | [RuntimeCoordinator](../../backend/src/runtime/coordinator.rs) 有候选/CAS/drain；[DnsService](../../backend/src/service.rs) 已差量复用、CAS 前预注册任务并按原请求 deadline drain | 沿既有 owner 补齐完整应用判定、进程 owner 补偿和控制命令，不再建另一套 Runtime |

本轮核对上述源码和原计划，保留 29 张图稿映射，并修订冲突的文字要求；未重新执行视觉验收、产品构建或运行测试。原文档既有证据不自动升级为本轮通过记录。

### 2.2 必须交付

- 12 个一级模块及对应配置弹窗；初始化、登录、登出和会话失效继续可用。
- 图稿明确的列表、新增和编辑入口；策略内规则/成员的增删改排序属于所属资源的一次完整编辑。
- 原始 ID/IP、历史匹配 ID/来源、当前名称的完整链路；各模块 name 唯一，客户端请求 ID 单独保留。
- 共享内存预算、可选独立二进制缓存快照、详情 UTC 日分片、统计/详情共同保留水位。
- 类型化配置查询、预校验、热应用后持久化、revision 冲突及受限系统配置；外部差异提示、还原/修改、未同步重试。
- 服务指标和解析记录 WebSocket，进程状态查询、断线恢复、有界缓冲及认证回收。
- 新版配置/数据直接初始化、故障恢复、Windows 约 10 客户端验证和缓存命中主链路 2ms 检查。

### 2.3 明确不纳入

- 通用 YAML/JSON 整份配置编辑器、任意文件读取/覆盖、Secret 明文查看或上传。
- 系统重启/停止按钮、用户和权限管理、多实例控制、资源强制刷新、协议探测工具。
- 独立缓存清空按钮、记录删除按钮、资源批量删除或未经设计的顶层资源删除流程。缓存失效与代际测试仍是后端内部正确性要求。
- 给 listener 擅自增加独立 `enable` 配置键，或增加当前模型没有的上游协议。
- 旧配置/旧数据库迁移、legacy 查询、旧版兼容与维护回退；历史身份推断、历史 IP 重匹配、静默清理不明目录。
- 与本次改造无关的依赖升级、全仓重命名或 DNS 算法替换。

## 3. 评审决策与开工门槛

已确认结果统一维护于[决策清单](webui-management-decisions.md)。本表保留决定到任务的路由，不再作为待用户逐项批准的列表；实施时将正式值落到 schema、源码契约和权威文档。

| 编号 | 决策入口 | 阻塞任务 |
| --- | --- | --- |
| D-01 | [版本与前后端切换](webui-management-decisions.md#d-01-版本与前后端切换) | BE-01、FE-01 |
| D-02 | [运行时优先与文件处理](webui-management-decisions.md#d-02-保存应用与待重启配置) | BE-02、FE-02、配置热更新专项 |
| D-03 | [资源改名与操作范围](webui-management-decisions.md#d-03-资源改名与本期操作范围) | BE-08、FE-05 至 FE-11 |
| D-04 | [指标口径与在线客户端](webui-management-decisions.md#d-04-指标口径与在线客户端) | BE-09、FE-03/11 |
| D-05 | [分页、自动刷新与缓冲](webui-management-decisions.md#d-05-分页自动刷新与实时缓冲) | BE-05/07/10、FE-04 |
| D-06 | [保留、快照与默认值](webui-management-decisions.md#d-06-保留策略缓存快照与默认值) | BE-01/04/06/08、FE-07 |
| D-07 | [配置路径与 SecretRef 可见范围](webui-management-decisions.md#d-07-配置路径与-secretref-可见范围) | BE-07/08、FE-09/10/11 |
| D-08 | [选型与依赖审批](webui-management-decisions.md#d-08-技术选型与依赖审批) | BE-06/09/10、FE-01/03/12 |
| D-09 | [主题与图稿外状态](webui-management-decisions.md#d-09-界面主题与图稿外状态) | FE-01/02/03/12 |
| D-10 | [取消旧版兼容](webui-management-decisions.md#d-10-旧配置数据迁移与回退) | BE-11 新基线、P5 |
| D-11 | [验收平台、规模与阈值](webui-management-decisions.md#d-11-验收平台规模与性能门槛) | BE-12、FE-12、GC-02 |
| D-12 | [启动范围与阶段提交](webui-management-decisions.md#d-12-启动范围与阶段性提交) | 相应实施与 Git 操作 |

D-01 至 D-12 的方向均已确认，T-01 至 T-08 按具体任务核定，不重复阻塞原方向。常见依赖已获本任务范围授权；明确启动实施后按检查点 commit，不 push。本轮只修订方案。

## 4. 开发顺序

### 4.1 阶段与完成门槛

| 阶段 | 后端工作 | 前端工作 | 离开本阶段的门槛 |
| --- | --- | --- | --- |
| P0 契约冻结 | BE-01；BE-11 新格式初始化规格 | FE-01 路由/图稿差异，FE-02 表单及应用/文件状态契约 | name/client_id、v2、单位、操作范围、失败语义固定；无迁移支线 |
| P1 公共底座 | BE-02 活动源/应用/持久化/watcher/日志；BE-03 身份；BE-09 指标可独立推进 | FE-01 壳层认证；FE-02 公共表单；FC-16 差异处理基础 | 正常热更新、失败事实、文件只提示、还原及日志关开可验证；mock 隔离 |
| P2 核心数据 | BE-04 快照；BE-05 分片后推进 BE-06 保留；BE-07 按就绪数据源分批完成 | FE-03/FE-04 在固定契约 fixture 下开发；FE-07/FE-11 做只读与编辑状态 | 快照与历史分离；分片和水位有真实 SQLite 测试；配置读接口不泄露秘密 |
| P3 配置模块 | BE-08 按下述依赖顺序逐组交付；CR-04 多模块采用随表单补齐 | FE-09/FE-10 基础资源，继而 FE-06、FE-08、FE-05 和客户端；FE-07/FE-11 随后端交付 | 每组读取、预校验、应用、持久化、冲突、外部差异处理及回显闭环 |
| P4 实时与查询 | BE-07 最终查询；BE-10 推送接入 BE-05 提交流和 BE-09 指标 | FE-03 实时服务状态；FE-04 实时记录、详情冻结、补齐；FE-11 进程信息 | 真 HTTP/WS 交错、重连、会话失效、保留清理和目录变更联合验证通过 |
| P5 验收与收口 | BE-11 新基线/旧路径退出；BE-12 Windows 组合与 2ms 检查 | FE-12 全模块浏览器、响应式、安全与内嵌交付验收 | 新版冷启/重启及关键失败矩阵通过；Linux 未实测单列，不阻塞；文档沉淀 |

P2、P3 中不互相依赖的分支可交错推进，但“可先做界面”不等于后端链路已交付。每次模块联调都使用同一 API 契约版本。

### 4.2 必须保持的依赖

```text
BE-01
  -> BE-02 -> BE-07 的配置读 -> BE-08 -> 各配置页面真实保存
  -> BE-03 -> BE-05 -> BE-06 -> BE-07 的最终历史查询
  -> BE-04 -> FE-07 的快照配置和状态
  -> BE-09 -> BE-10 的指标通道 -> FE-03 实时状态
BE-05 + BE-07 -> BE-10 的记录通道 -> FE-04 实时记录
FE-01 -> FE-02 -> 各配置页面
BE-03/04/05/06 + BE-11 -> BE-12 + FE-12 -> 联合交付
```

配置模块的联调顺序：

1. 全局 DNS 基础配置、代理、Hosts、规则集，先建立可供引用的资源。
2. Hosts/DoH 上游，再建立上游组；`bootstrap` 和嵌套组按完整引用图校验。
3. 策略及其有序规则，引用前两步已有的资源/上游。
4. listener/DoH 路由及客户端覆盖，引用已完成的策略。
5. DNS 统计保留和详情开关在 BE-06/BE-08 就绪后收口；日志编辑可在 BE-02 后独立验证。

以上是新建演练数据的顺序，不限制读取现有合法配置。不可通过临时保存无效引用绕过完整候选校验。

### 4.3 原方案步骤对应

| 原方案阶段 | 本次详细任务 |
| --- | --- |
| B1 契约与新基线 | BE-01、BE-11 新版初始化、D-01/D-06/D-10 |
| B2 原始身份链路、B3 匹配结果与查询显示 | BE-03、BE-07、FE-04、FE-10 客户端 |
| B4 缓存快照 | BE-04、FE-07 |
| B5 详情日分片、B6 保留协调器 | BE-05、BE-06、FE-04、FE-07、FE-11 |
| B7 管理写入与推送 | BE-02、BE-07 至 BE-10、FE-01 至 FE-11 |
| B8 集成与文档收口 | BE-11、BE-12、FE-12 及本文联合验收 |

## 5. 跨端契约交接

每个模块进入联调前，后端交付正式 schema/生成类型、合法与非法 fixture、字段错误路径、revision/应用模式和已执行测试；前端交付状态清单、表单序列化测试、请求取消与缓存失效规则。接口字段以当批正式 OpenAPI 为唯一权威，计划只保留工作分解。

| 主题 | 后端责任 | 前端责任 |
| --- | --- | --- |
| 配置读取 | 当前活动源表达、继承来源、生效摘要、引用和 active/file revision | 以活动源初始化，不把外部文件或 resolved 值当草稿 |
| 配置保存 | 旧 name、预期版本；先应用后写文件，返回 operation 与真实状态 | 保留草稿，结果未知先查询，不重放命令 |
| 文件变化 | 仅检测、脱敏差异、还原/组合采用、覆盖 CAS 及同步重试 | 全局提示、模块化差异工作区、覆盖未采用内容确认 |
| 生效可见性 | active/persisted revision、任务边界 revision | “已应用未同步”不同于成功；下一周期任务状态单列 |
| 单位与缺数 | 字节、毫秒/微秒、UTC 时间及单位、窗口/采样时间明确；缺数返回原因 | 按安全精度转换，不用零填空，不把 MB/MiB 或 QPS/RPM 混为一谈 |
| 详情 | 原始身份、匹配结果、当前目录投影及有界 Answer，不保留旧版 legacy 支线 | 不改历史、不补造信息；浮层冻结记录及当次名称 |
| 保留 | 后端计算预览、真实水位与可查范围，执行时重新采样 | 缩短影响先确认；不由浏览器计算权威截止日期 |
| 推送 | 认证、快照边界、连续序列、缺口、有限补齐、慢消费者保护 | 同过滤订阅，去重、取消、有限缓冲、resync；关闭刷新不继续更新 |
| 安全 | 受限 DTO、同源防护、写权限和安全错误 | 文本渲染，不持久化凭据/敏感草稿，不把系统只读当纯 UI 限制 |

契约变更必须同批修改 Rust port/DTO、OpenAPI、生成类型、API client、fixture 和关联测试。不得靠 `any`、手写生成类型、双字段猜测或成功提示掩盖未接线能力。

## 6. 联合验收矩阵

下列编号用于 BE-12/FE-12 汇总证据，不表示本轮已经执行。

| 编号 | 场景 | 通过条件 |
| --- | --- | --- |
| E2E-01 | 初次配置、登录、导航、登出 | 12 个模块受保护；会话过期清理 HTTP/WS 和前一用户数据；错误不误判为空数据 |
| E2E-02 | 新建依赖链及上游改名 | 代理/资源 -> 上游/组 -> 策略 -> listener/client；旧名称定位且引用同批更新，无悬空配置 |
| E2E-03 | 并发编辑和外部文件变化 | 不自动 reload；提示/还原/组合采用；普通保存覆盖确认；二次外改冲突；应用后写失败可重试 |
| E2E-04 | 保存后实际 DNS 生效 | UDP/TCP/DoH 读取新策略；失败候选保持旧服务；图中保存成功不能代替真实 DNS 响应 |
| E2E-05 | 身份与历史 | ID 优先、未知 ID 回退 IP、无 ID、CIDR 包含；name 唯一，改名不改历史匹配；无 legacy 支线 |
| E2E-06 | 缓存快照 | 冷启/恢复/损坏/权限失败、预算改变、停机 TTL、reload/关闭交错；不影响统计/详情 |
| E2E-07 | 保留和日分片 | R/G/T 边界、跨 UTC 日、服务器 01:00、补跑、lease/迟到写入、失败重试；逻辑可见与物理释放分开 |
| E2E-08 | 实时记录与详情 | 快照/订阅无竞态缺口；详情保持同一 ID；缓冲满和断线显式重同步；同时间、迟到记录不漏不重 |
| E2E-09 | 指标与进程状态 | 受控请求数可核算 QPS/RPM/在线身份；两页面内存口径相同；窗口未满、断流和缺数不伪造 |
| E2E-10 | 安全边界 | 伪造跨源写入/WS、会话撤销、只读字段注入、超限载荷及恶意文本均受控；Network/Storage/日志无 Secret 实际值 |
| E2E-11 | 新基线与恢复 | 新目录初始化、新版重启读取、journal/分片恢复；旧格式明确拒绝，无自动搬迁或清库 |
| E2E-12 | UI 与交付 | 29 张图稿映射均有去向；桌面/窄屏/键盘/触屏、生产无 mock、内嵌 SPA 与未知 API 正确分流 |
| E2E-13 | Windows 低并发与核心时延 | 约 10 客户端、release/预热缓存；DNS core 命中耗时 2ms 内，排除客户端 I/O；检查后台任务不阻塞主链路 |

2ms 测点、分位和超时样本的处理以[决策 D-11](webui-management-decisions.md#d-11-验收平台规模与性能门槛)为准；不得替换为单次 cache lookup 或自行降为平均指标。真实 Windows 文件、SQLite、HTTP/WS 和浏览器是门槛；其他平台代码记录审查/未实测，不要求另行平台验收或超高负载压测。新格式恢复与安全测试不因取消兼容而省略。

## 7. 交付控制与文档退出

### 7.1 交付检查单

- [ ] 决策清单 D-01 至 D-12 的相关决定和 T-01 至 T-08 的必要核定已落实，schema 与任务依赖一致。
- [ ] BE-01 至 BE-12、FE-01 至 FE-12 的必要实施和验收全部有证据。
- [ ] E2E-01 至 E2E-13 已记录基线、环境、实际命令、结果与未覆盖边界。
- [ ] 旧版迁移/兼容支线退出，新格式冷启/重启和故障恢复通过；没有清理未指定的本地目录。
- [ ] 前后端同版本构建和发布手册就绪，无未经授权的安装、push 或部署。
- [ ] 实现文档、接受的架构、配置参考、示例、schema、生成入口和索引同批更新。

### 7.2 文档沉淀

实际完成的配置契约归入[配置参考](../implementation/configuration.md)；身份与请求链路归入[DNS 管线](../implementation/backend/dns-pipeline.md)；快照、分片及保留归入[后台服务](../implementation/backend/background-services.md)；写入/推送归入[Management 实现](../implementation/backend/management.md)；页面和状态归入[前端实现](../implementation/frontend/README.md)；构建切换归入[交付实现](../implementation/delivery.md)。

改变的设计同步对应 [Config](../architecture/backend/modules/config.md)、[Policy](../architecture/backend/modules/policy.md)、[Cache](../architecture/backend/modules/cache.md)、[Storage](../architecture/backend/modules/storage.md)、[Management](../architecture/management.md) 和[前端设计](../architecture/frontend.md)。完成计划及其索引项在同一批次删除，不新建 archive/history；图稿按长期设计价值迁移或删除。

只有代码完成但必要验收缺失时，保留活动计划并标为待验收，不把计划提前写入当前实现作为通过事实。

## 8. 阶段性 Git 提交

### 8.1 提交原则

本项目重构必须通过多个有明确验收范围的提交推进，**不得把 P0-P5、整个后端或整个前端攒成一个 Git 提交**。阶段是开发/联调门槛，提交是可审查的语义单元，两者不是一一对应；每个阶段通常包含多个提交。

- 后端按[BC-01 至 BC-32](webui-management-backend-development-plan.md#15-后端阶段性提交检查点)推进，前端按[FC-01 至 FC-16](webui-management-frontend-development-plan.md#16-前端阶段性提交检查点)推进，加上 GC-01/02/03 共 51 个建议检查点；仍过大的任务继续拆分。
- 一个提交围绕一个可说明的目标，包含直接关联的实现、schema/生成类型、测试、示例和文档；不要按文件后缀拆成“代码先提交、类型和测试以后再补”。
- 每次提交后相关工程必须可编译/类型检查，已接线能力的针对性测试通过。跨模块契约变化无法安全拆开时，将最小闭合集合放在同一提交，不故意制造中间坏版本。
- 前置能力可以先提交内部类型、adapter、组件和测试，再在后续提交切换生产装配/路由；明确未接线边界，不增加无必要的长期兼容层或临时产品配置开关。
- 明确启动实施后，单个检查点完成最小验证即精确暂存/提交，不等待整阶段结束；用户已确认阶段性 commit，不需对获准阶段内每一笔重复请示。
- 使用简洁的一行中文 Conventional Commit。后端/前端表中的提交信息是建议，必须按真实 diff 调整，不将未完成的工作写成已实现。
- 不为已有变更历史做整套 squash；不自动执行 rebase、revert、reset、push、tag 或发布。

原计划修订已在 `007f943` 完成；本次只执行获准的 P0 检查点，实施及提交遵循[决策 D-12](webui-management-decisions.md#d-12-启动范围与阶段性提交)，commit 不包含 push，不自动开始 P1。

### 8.2 最小验证级别

| 级别 | 提交前最低要求 |
| --- | --- |
| V-D 文档 | 文档检查器、`git diff --check`、链接/任务依赖与实际 diff 人工复核 |
| V-B 后端 | Rust fmt check、受影响 crate/测试目标可编译、定向单元/契约测试；共享契约变化扩大测试范围 |
| V-A API | Rust 实际 DTO/handler 契约测试、OpenAPI 生成类型、前端 typecheck 与受影响 contract/fixture 测试 |
| V-F 前端 | typecheck、受影响 Vitest/组件测试；路由、公共样式、依赖或构建配置变化追加 build |
| V-I 实际集成 | 该提交涉及的真实 SQLite/文件/OS/HTTP/WS 或浏览器定向验证；仅适用于本次边界，完整平台与负载矩阵仍留阶段门槛 |

执行命令、目录、工具链和环境条件见子计划，未执行的验证不得勾为通过。Windows 必需场景未通过时保留待验收；仅缺 Linux 等其他平台实测不阻塞 P5，单列未验证边界。

### 8.3 阶段与提交顺序

| 阶段 | 推荐提交检查点 | 阶段停靠点 |
| --- | --- | --- |
| P0 | GC-01 修订计划/决策；BC-01 契约；BC-26 新格式初始化可先行 | 契约和提交范围可审查，无迁移预览 |
| P1 | BC-02/03/29 内部能力已提交，生产闭合待 BC-30；BC-31 热日志、BC-04/05 身份与 BC-23 指标已提交；FC-01/02 与 FC-16 可独立基础已提交 | 应用/持久化/watcher 分别提交，基础可供模块使用；生产闭合转入依赖链 |
| P2 | BC-06/07 快照；BC-08/09 分片；BC-10/11 保留；BC-12/13 查询 | 每条数据分支独立提交，真实 adapter 通过后再交给页面联调 |
| P3 | BC-14 至 BC-22；FC-12/09/10 -> FC-06 -> FC-08 -> FC-05/11；FC-07/13；FC-16 随表单补齐，最后 BC-32 | 每模块单独闭环；多模块差异采用和配置故障回归独立提交 |
| P4 | BC-24/25 实时通道；FC-03/04 实时页面；FC-14 进程状态已提前交付；必要的联调修复分别提交 | 真 HTTP/WS 和页面语义闭合，模块改动已有历史节点 |
| P5 | BC-27 旧路径退出；BC-28 Windows 集成；FC-15 旧 UI 退出；GC-02 联合验证；GC-03 文档收口 | 新基线/安全/配置同步/2ms 验收完成，不用最终提交代替开发历史 |

同一行内只代表可以处于同阶段，实际依赖仍以上下游表格为准。没有必要的新改动时不为“打点”创建空提交；联合验证发现缺陷时先提交具体修复及回归测试，不能只提交一个通过说明。

### 8.4 跨端与收口检查点

| 检查点 | 提交范围 | 前置和验证 | 建议提交信息 |
| --- | --- | --- | --- |
| GC-01 | 本轮重订计划、配置专项、确认结果、原方案和索引 | V-D；本轮完成后复核；不是重复创建先前已有提交 | `docs(webui): 按确认决策重订前后端重构计划` |
| GC-02 | 实际新增的联合回归用例及可追溯验证说明；缺陷修复另按语义提交 | BC-28、FC-15；总计划 E2E 矩阵完成；V-B/V-F/V-A/V-I/V-D 按实际范围 | `test(webui): 补齐前后端重构联合验收` |
| GC-03 | 剩余跨领域稳定文档整合、完成计划/评审图稿退出和索引修正 | GC-02；实施及必要验收完成；不得代替每个开发提交的直接文档同步；V-D | `docs(webui): 收口重构文档与活动计划` |

### 8.5 提交操作与记录

每个检查点实际执行时：

1. 核对当前任务范围、分支、未暂存/已暂存/未跟踪文件，保留无关工作。
2. 执行该检查点最小验证并记录结果；检查真实 diff，确定提交闭合集合。
3. 已有范围授权时精确暂存/提交；没有授权时报告建议 subject、路径和验证，等待批准。已完整暂存则不重复 `git add`，不使用 `git add .` 混入其他模块或本地数据。
4. 提交后记录 commit hash、subject、完成的检查点、验证边界及剩余工作；阶段完成与单次 commit 完成分别报告。

`_fluxdns/` 测试数据、秘密、构建物与缓存不得提交。真实发布依然按独立授权和交付流程执行。
