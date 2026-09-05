# FluxDNS 文档结构与维护机制调整方案

> 文档状态：草案
>
> 实现状态：部分实现
>
> 适用范围：项目 plan、架构设计、当前实现文档的分类、迁移、权威边界与维护流程；不改变产品行为
>
> 最后核对：2026-09-05
>
> 代码核对基线：`c400d335c7906add4a9c56adbe85781ae9db4482`
>
> 关联文档：[文档总索引](../README.md) · [现行文档维护规范](../rules/documentation-maintenance.md) · [项目文档维护 skill](../../.agents/skills/project-doc-maintenance/SKILL.md)

## 1. 结论与需求解释

建议把技术文档的一级分类从“后端、前端”调整为“计划、架构设计、当前实现”，再在分类内部按领域或实际链路组织。用户已确认规则层使用 `rules/`，相关目录迁移已在本轮完成；其余分类和内容拆分仍待评审。规则层不承载三类技术内容，也不参与功能进度统计。

本文将需求中的“现有代码实时”理解为“现有代码实现文档”：说明当前代码到底实现了什么、生产入口是否接入、还有哪些限制以及验证到什么程度。“实时”通过代码变更时同步维护实现，不表示自动监控代码、后台生成文档或对生产运行状态作实时承诺。

核心决策：

1. `docs/plans/` 回答“准备改变什么、如何实施、怎样验收”，不再恢复已删除的前后端综合开发计划。
2. `docs/architecture/` 回答“为什么这样设计、各部分有哪些稳定约束”。
3. `docs/implementation/` 回答“当前代码如何运行、实际支持什么、证据和限制是什么”。
4. `docs/rules/` 回答“开发和文档维护必须怎样执行”，目录与引用迁移已完成，具体维护逻辑调整仍待批准。
5. 按段落职责拆分旧文档，不把整篇混合文档简单改名为架构或实现文档。
6. 分开表达设计是否接受、功能是否落地和测试是否完成，不再让一个“部分实现”同时表示这三件事。

目录继续使用已有的复数 `plans/`；需求中的 plan 是文档类别，不需要为单复数命名再做一次迁移。

本轮输出方案并更新方案索引，同时按用户追加授权完成规则目录迁移及入站引用更新。除此之外，以下目录、模板、脚本和内容拆分均为拟实施内容，尚未生效；规则文档中的原有要求保持不变。

## 2. 当前问题与代码依据

本次已检查现有文档目录、维护规范、模块索引，并静态追踪后端正式启动、核心构造、Management 路由和前端鉴权/页面入口。没有运行编译、单元测试、网络 smoke 或发布流程，因此下列代码依据不等于本轮运行验证。

| 问题 | 当前证据 | 对方案的影响 |
| --- | --- | --- |
| 架构、事实、任务混写 | [后端架构](../backend/architecture.md) 同时包含技术选型、模块边界、缓存实现和“推荐实现顺序”；[前端架构](../frontend/architecture.md) 同时包含页面现状、历史验证结果和阶段 A-D | 按内容职责拆分，而不是只改目录名 |
| 状态摘要已经漂移 | 根 [README](../../README.md) 第 15 行仍称上游未执行出站 I/O、Moka/SQLite persistence 和完整管线未完成；正式 `run_command` 已调用 remote-resource prepare，后者构造 `PolicyDnsCore` 并初始化 cache persistence | 当前实现必须回查正式入口，不能照抄旧摘要；根 README 不再维护逐模块进度和测试流水账 |
| 缺少独立的实际接线说明 | [app.rs](../../backend/src/app.rs) 装配 Storage、RuntimeCoordinator、Management 和 DnsService；[service.rs](../../backend/src/service.rs) 持有 transport/resource task 与 ResolutionRuntime；模块索引仍以早期 12 个设计模块为主 | 补充生命周期、后台任务和 Management 的实现入口，不强制文档与源码文件一一对应 |
| plan 承载长期契约 | [v2 整合方案](webui-v2-management-integration.md) 仍详细描述路由、安全、配置事务和静态资源；[Management router](../../backend/src/management/router.rs)、[ConfigStore](../../backend/src/config/store.rs) 已有相应实现 | 已落地契约和代码事实应退出 plan；剩余环境验收仍可保留为有界任务 |
| “实现状态”混入测试缺口 | [Cache 文档](../backend/modules/cache.md) 头部为“部分实现”，正文已描述 Moka/SQLite、optimistic refresh 和生命周期，剩余边界包含真实 disk-full 故障测试 | 功能状态与验证状态分栏，不能把缺测试等价为没实现 |
| 前端缺少独立实现导航 | [App.tsx](../../frontend/src/app/App.tsx) 已有 `/initialize`、`/login` 和七个受保护页面；[AuthProvider](../../frontend/src/modules/auth/AuthProvider.tsx) 已按 setup 状态启用 session query | 架构保留依赖和安全原则；具体路由、查询和状态流进入实现文档 |
| 字段已有机器可读权威 | [OpenAPI](../../frontend/openapi/management-api-v1.yaml) 定义接口；[generated.ts](../../frontend/src/shared/api/generated.ts) 明示自动生成；[package.json](../../frontend/package.json) 已定义 `generate:api` | 保留 schema 和生成链位置，Markdown 不再完整抄写字段定义 |

关键后端追踪链：

```text
app::run_command
  -> PreparedRuntime::prepare_with_policy_core_and_remote_resources
  -> PolicyDnsCore::from_config_with_resource_snapshots
     -> UpstreamRegistry::from_resolved_with_outbounds
     -> MokaCacheStore / initialize_cache_persistence
  -> StorageRuntime::open -> bind_prepared -> RuntimeCoordinator
  -> ManagementService::bind
  -> DnsService::with_default_timeout_from_coordinator_*
     -> ResolutionRuntime / transport tasks / resource tasks
  -> attach_management
```

对应入口见 [prepared.rs](../../backend/src/runtime/prepared.rs)、[dns/policy.rs](../../backend/src/dns/policy.rs)、[upstream/registry.rs](../../backend/src/upstream/registry.rs) 和 [resolution.rs](../../backend/src/resolution.rs)。其中 Reqwest DoH transport、Moka 构造与 SQLite 恢复均有实际构造路径；本次未验证远程可用性、吞吐量或所有故障矩阵。

这些证据足以确认文档职责需要调整，但不是对全部配置字段、模块和安全契约的完整审计。迁移时仍须逐段核对。

## 3. 分类与权威边界

### 3.1 三类主体文档

| 类别 | 必须包含 | 不应包含 | 更新触发 |
| --- | --- | --- | --- |
| plan | 问题、代码基线、目标/非目标、候选变更、实施步骤、风险、验收和退出条件 | 全项目完成比例、已实现功能流水账、长期完整字段契约 | 需求评审、范围调整、实施和验收推进 |
| 架构设计 | 已接受的设计决策、职责、依赖方向、不变量、失败语义及取舍 | 当前函数清单、测试次数、逐阶段任务、未批准的远期设想 | 设计决策、跨模块契约或重要失败语义改变 |
| 当前实现 | 正式入口、关键调用链、状态/持久化路径、能力边界、源码/测试依据 | 拟议实现、未经核验的“已支持”、重复的设计论证 | 代码行为、接线路径、接口、配置或验证边界改变 |

同一主题可以同时有设计和实现文档，但不能完整维护两份同一事实。例如：

- 架构说明 Management 与 DNS 数据面隔离的原因和边界；实现说明 `ManagementService` 在哪里创建、如何注入读取依赖、由谁停止。
- 架构说明配置事务不得回写解析后的 SecretRef 值、两个文件不具备单次原子替换；实现说明 `ConfigStore`、`source_edit`、journal 和启动恢复的实际路径。
- 架构说明 cache commit 不阻塞响应和质量更新约束；实现说明 `PolicyDnsCore`、finalizer、ResolutionRuntime 与 persistence 当前如何接线。

未批准的设计留在 plan。已批准但尚未落地的设计可以进入架构文档，但必须明确标注适用边界并链接活动 plan；当前实现文档只能陈述已有代码，不得借此宣称功能已交付。

### 3.2 事实与契约冲突

| 对象 | 权威定位 |
| --- | --- |
| 当前可执行行为 | 源码、实际调用链与对应配置；测试/运行结果说明验证覆盖，不能替代接线核对 |
| 已接受的预期契约 | 有效架构设计与接口 schema；代码偏差应显式记录，不因代码存在就自动改写预期 |
| Management API 字段和类型 | `frontend/openapi/management-api-v1.yaml`；生成类型和 client/handler 必须与之对齐 |
| 当前配置字段参考 | 拟议的 `implementation/configuration.md` 是唯一完整人工参考，逐项对照 DTO、resolve、validate、示例和运行时使用位置 |
| 工具版本和构建规则 | `mise.toml`、Cargo/package manifest、脚本/workflow；规范约束使用方式，实现文档描述当前交付链 |
| 任务进度 | 对应活动 plan，不另建前后端综合进度文档 |

发现冲突时，先判断是文档陈旧、实际代码偏差还是契约待决。迁移任务不得擅自修改源码来让它符合旧文档，也不得把未接受的代码偏差写成新的有效设计。未决事项写明两侧证据和最小决策点；与迁移无关的修复另行授权。

## 4. 目标目录

以下是本次迁移的目标职责树，不是需要立即创建的占位文件清单。只有完成对应内容拆分后才创建文档；不创建空目录。

```text
docs/
|-- README.md
|-- plans/
|   |-- README.md
|   |-- documentation-structure-and-maintenance.md
|   `-- webui-v2-management-integration.md
|-- architecture/
|   |-- README.md
|   |-- system.md
|   |-- management.md
|   |-- frontend.md
|   `-- backend/
|       |-- README.md
|       |-- overview.md
|       `-- modules/
|           |-- README.md
|           `-- <existing-module>.md
|-- implementation/
|   |-- README.md
|   |-- configuration.md
|   |-- delivery.md
|   |-- backend/
|   |   |-- README.md
|   |   |-- lifecycle.md
|   |   |-- dns-pipeline.md
|   |   |-- background-services.md
|   |   `-- management.md
|   `-- frontend/
|       |-- README.md
|       |-- application.md
|       `-- pages.md
`-- rules/
    |-- README.md
    |-- documentation-maintenance.md
    |-- environment-usage.md
    `-- local-testing.md
```

### 4.1 目录内的具体职责

- `architecture/system.md`：前后端边界、DNS 数据面与 Management 控制面、开发/内嵌部署模型；不重复模块算法。
- `architecture/backend/overview.md`：domain/ports/adapter 依赖、运行时所有权和跨模块不变量；模块内部设计继续放在 `modules/`。
- `architecture/management.md`：跨前后端的同源、认证、首次初始化和 API 隔离设计；配置事务的不变量引用 Config 模块设计，字段引用 OpenAPI。
- `architecture/frontend.md`：应用分层、server state、路由守卫和 API client 的设计约束，不维护页面实现清单。
- `implementation/backend/README.md`：实际源码边界到实现文档的导航表，包含 `app`、`service`、`resolution`、`management`；不维护整体完成比例。
- `implementation/backend/lifecycle.md`：配置加载、prepare/bind/activate、reload、监督与关闭的实际接线。
- `implementation/backend/dns-pipeline.md`：入站到 Policy/Cache/Upstream 的请求路径、分支和回传，不复制完整算法设计。
- `implementation/backend/background-services.md`：资源刷新、resolution 分发、cache commit、Storage writer、Telemetry 的后台链路、队列和生命周期。
- `implementation/backend/management.md`：实际路由、依赖注入、认证会话、ConfigStore 接线及读取 adapter；接口字段不重复 OpenAPI。
- `implementation/frontend/application.md`：入口/providers、setup/session 状态流、API client、缓存与 mock 开关。
- `implementation/frontend/pages.md`：实际页面路由、数据来源、交互边界、刷新与组件测试入口；相同页面模式使用表格，不为七个页面机械建七份文档。
- `implementation/configuration.md`：当前版本字段、路径、校验和支持边界；区分“可解析”和“运行时已接入”。
- `implementation/delivery.md`：前后端构建、内嵌打包、版本脚本、服务脚本和自动发布的当前行为及验证限制。

保留现有 12 份模块设计的有价值内容；不强制同时生成 12 份同名实现文档。实现文档以后只有在链路内容确实无法清晰维护时才按稳定边界拆分，不按源码文件数或历史任务数拆分。

根 `README.md` 仍是面向使用者的简介、最短使用入口和文档导航；`frontend/README.md` 保留前端工程快速入口。`AGENTS.md` 保留强制路由与安全边界。它们不构成另外三套实现说明。

### 4.2 规则层命名

推荐 `rules/`，中文标题为“协作与工程规则”。这是对规则强制性和适用对象的明确命名，不是新增一套制度。

| 候选 | 判断 |
| --- | --- |
| `rules/` | 推荐；适合工具安装边界、必查文档、产物位置和提交前检查等必须执行的规则 |
| `standards/` | 可以理解，但更偏标准规范，不能直接说明这里保存的是项目协作规则 |
| `guidelines/` | 更像建议性指南，不适合作为强制规则的统一名称 |
| `governance/` | 对当前仓库过于宽泛，容易引入权限、组织、审计等无关职责 |

`docs/rules/` 只描述人的协作和工程操作规则。DNS 的 rule、rule-set、Policy 仍属于架构和实现文档，不能因为名称相似移入这里。各文件继续使用 `documentation-maintenance.md`、`environment-usage.md`、`local-testing.md`，避免无意义地同时改目录名和文件名。

## 5. 内容迁移方案

### 5.1 现有文档去向

规则目录本身已迁移；以下内容拆分、其他目录迁移及规则正文重写必须在实施获批后执行。保留的是事实和设计价值，不是所有历史措辞。

| 当前对象 | 目标位置或处理方式 | 拆分要求 |
| --- | --- | --- |
| `docs/backend/architecture.md` | `architecture/system.md`、`architecture/backend/overview.md` 和对应实现链路文档 | 分离系统边界、后端契约、实际接线与验收记录；移除已完成的实施步骤 |
| `docs/backend/modules/*.md` 中的 12 份模块设计 | `architecture/backend/modules/` 的同名文档 | 保留职责、不变量、算法取舍和失败契约；把当前实现边界、真实文件映射与验证缺口提取到实现文档 |
| `docs/backend/configuration-reference.md` | `implementation/configuration.md` | 保留唯一完整字段参考；检查每项是否仅能解析、已经接线或仍受限制；原理说明引用设计 |
| `docs/frontend/architecture.md` | `architecture/frontend.md`、`implementation/frontend/application.md`、`implementation/frontend/pages.md` | 分离设计、实际路由/状态流、验证事实；不保留已完成的阶段 A-D 任务表 |
| `docs/plans/webui-v2-management-integration.md` | 仍留在 `plans/`，但收敛为剩余任务 | 把长期设计移入 `architecture/management.md` 或相应模块设计，代码事实移入 Management、前端和 delivery 实现文档 |
| `docs/rules/documentation-maintenance.md` | `rules/documentation-maintenance.md` | 重写分类、权威矩阵、状态、维护触发和生命周期，不复制本方案的调查过程 |
| `docs/rules/environment-usage.md` | `rules/environment-usage.md`，部分内容提取到 `implementation/delivery.md` | 工具来源、安装限制、执行目录和缓存边界留在规则；脚本内部流程、发布行为及平台限制进入实现 |
| `docs/rules/local-testing.md` | `rules/local-testing.md` | 本地目录、测试操作和证据记录要求留在规则；脚本具体行为引用 delivery，不维护两份 |
| `docs/backend/README.md`、`docs/frontend/README.md`、原模块索引 | 对应新分类索引 | 路由迁移后删除失去职责的旧索引和空目录，不创建跳转副本 |
| `docs/rules/README.md` | 保留原位 | 在规则内容拆分后更新职责描述和入口 |
| 根 `README.md` | 保留原位并精简 | 删除漂移的逐模块进度与历史测试长段；保留经核对的用户级摘要和入口 |
| `frontend/README.md` | 保留原位并更新链接 | 保留快速开发/生成命令；具体实现和发布流程链接到唯一权威位置 |
| 根 `AGENTS.md` 与项目文档维护 skill | 保留原位并更新路由、分类决策链 | 强制读取路径必须与新目录一致；skill 不再复制完整规范表和状态定义 |

上述相对目标均位于 `docs/`。现有模块文件名保留 `application`、`config`、`ports`、`dns-core`、`policy`、`cache`、`upstream`、`transport`、`resource`、`runtime`、`storage`、`observability`，不因迁移顺带重命名源码或技术概念。

### 5.2 当前实现的源码映射

| 实现文档 | 主要代码范围 | 需要回答的问题 |
| --- | --- | --- |
| 后端 lifecycle | `main.rs`、`app.rs`、`service.rs`、`runtime/*`、`config/load.rs` | 正式入口用哪条 prepare 路径；配置何时生效；谁持有任务；如何 reload 和停止 |
| 后端 dns-pipeline | `transport/*`、`ports/inbound.rs`、`dns/*`、`policy/*`、`cache/*`、`upstream/*` | 哪些 adapter 真正接入；请求经过哪些分支；命中、超时、取消如何结束 |
| 后端 background-services | `resolution.rs`、`resource/*`、`cache/runtime.rs`、`storage/*`、`observability.rs` | 哪些工作离开请求路径；事件如何分发；队列满、持久化失败与关闭的实际处理 |
| 后端 management | `management/*`、`ports/management.rs`、`storage/management_read.rs`、`config/store.rs`、`config/source_edit.rs` | 路由实际存在吗；数据从哪里读取；setup 写入与 session 更新如何协调 |
| 前端 application | `main.tsx`、`app/*`、`modules/auth/*`、`shared/api/*`、`mocks/*` | setup/session/401 的实际顺序；请求缓存与 mock 的边界 |
| 前端 pages | 七个业务页面及各自 `api.ts`、`hooks.ts`、组件测试 | 路由、数据来源、刷新、空/错状态和可用操作是什么 |
| configuration | `config/model.rs`、`resolve.rs`、`validate.rs`、`migrate.rs`、字段实际消费者与根示例 | 字段是否接受、如何解析、由谁使用、有什么运行限制 |
| delivery | `script/*.ps1`、`.github/workflows/release.yml`、Cargo/package manifest、Vite 配置 | 本地构建与发布行为有什么差异；输出到哪里；哪些环境未验证 |

这些是文档覆盖范围，不是要求复制源码目录树。跨文档共享的契约只引用，当前数字常量和字段限制优先引用源码/schema，不维护多张独立参数表。

### 5.3 v2 plan 的专项处理

v2 plan 不能因为代码文件存在就直接删除，也不能因为剩余环境验收而永久承载整套架构：

1. 对照实际 handler、ConfigStore、前端和发布脚本，提取已经接受的设计及已有实现。
2. 把“新增某模块”“建议引入某库”等已过时任务描述删除，不迁入当前实现文档。
3. 把仍有效的浏览器观察、平台发布或故障矩阵等验收要求压缩为任务表；原文状态仅是待复核线索，不能当作本轮验证结果。
4. 每个未完成项写明验收对象、所需环境、完成证据和退出条件，不再保留已完成步骤的流水账。
5. 剩余任务完成或由用户明确取消后，删除 plan 及索引项；不因“整理文档”自行宣布验收通过或取消验收。

## 6. 状态与内容模板

### 6.1 公共字段

所有文档继续保留 `文档状态` 和 `适用范围`。`文档状态` 仍使用“草案 / 有效 / 已废弃”，表示文档是否被接受，不表示代码成熟度。目录和标题已经表达分类，不再额外要求一份重复的分类清单。

索引仅维护导航、职责和必要的 plan 状态，不复制各功能的实现状态、完成比例和测试计数。

### 6.2 plan

批准新规范后，plan 用 `计划状态` 替代笼统的 `实现状态`：

```text
待评审 -> 待实施 -> 实施中 -> 待验收 -> 已完成
                         任一阶段 -> 已取消
```

“已完成 / 已取消”是清理前的终态，不是长期归档目录。待评审 plan 的文档状态为草案；获批后才能转为有效和待实施。代码完成但缺少要求的运行验收时进入待验收，不回退为“代码未实现”。

plan 头部记录代码基线、适用范围、计划状态；正文固定回答问题、现状依据、目标/非目标、变更设计、步骤、风险、验证和退出条件。索引与计划状态同步更新。

本方案当前仍使用现行模板；实施新规范时再与其他 plan 一并转换，避免草案提前成为规则。

### 6.3 架构设计

架构头部保留文档状态、适用范围和最后评审日期；不维护总体完成比例或“已实现/部分实现”。正文使用：设计结论、职责/依赖、核心不变量、关键流程和失败语义、取舍、实现文档入口。

源码重命名不必改变设计；契约变化必须重新评审。纯移动或排版不更新最后评审日期。已接受但未落地的设计必须有明确边界说明和活动 plan 链接，禁止把架构文件当作未来功能愿望清单。

### 6.4 当前实现

头部记录文档状态、适用范围、最后核对日期和核对基线；基线使用本次检查的源码提交，不要求填写尚未创建的文档提交哈希。源码链接附近标注关键符号名，避免只靠易漂移的行号定位。

正文至少包含入口与调用链、实际状态/数据流、能力边界、证据和已知限制。能力表分别记录：

| 能力 | 代码实现 | 正式入口接线 | 验证证据 | 已知限制 |
| --- | --- | --- | --- | --- |
| 某项能力 | 对应源码路径和符号，或明确未实现 | 说明调用位置、启用条件，或明确未接入 | 静态检查 / 定向测试 / 运行验证，附范围与日期；未执行时直说 | 未覆盖场景、平台或功能差距 |

表格只选择容易误解的关键能力，不为每个函数维护状态。函数存在、mock 通过、实际入口接入、真实环境通过是不同证据，不能互相替代。

只有回查受影响源码后才更新核对日期；改标题、迁移文件或更新不相关段落不能把整篇文档标成“最新”。历史测试结果必须保留其日期、环境和基线；缺少可追溯证据时写“原文报告，本轮未复核”，不能迁移为新通过记录。

## 7. 维护规范逻辑

### 7.1 文档选址决策

```text
是否规定协作、工具、目录或验证必须怎样执行？ -> rules
是否准备改变现有系统、且需要实施/验收？       -> plans
是否解释已接受的设计决策或稳定约束？         -> architecture
是否说明已有代码、实际接线和能力边界？       -> implementation
是否只是导航或最短使用入口？                 -> 最近一级 README
是否只是临时日志、个人配置或一次性输出？     -> _fluxdns 或任务记录
```

一个需求可能同时更新多类文档，但同一段内容必须有一个明确归属。plan 不再限于跨前后端需求：单模块的重要行为变更同样可以有 plan；简单修复不必先创建文档。

### 7.2 与代码一起维护

| 变更类型 | 必查对象 | 条件性同步 |
| --- | --- | --- |
| 功能、调用链、生命周期或失败行为变化 | 对应 implementation 文档及源码/测试 | 改变稳定契约时更新 architecture；存在活动 plan 时更新剩余任务 |
| 只有内部重命名或重构 | 实现文档中的源码链接、符号和接线描述 | 设计不变时不重写 architecture；不相关实现文档不机械刷新日期 |
| 配置字段、默认值、路径或校验变化 | model/resolve/validate、示例、implementation/configuration.md | 改变设计原则时更新 Config 设计；相关消费者和测试一起核对 |
| Management API 变化 | OpenAPI、生成类型、handler/client、实现文档和契约测试 | 认证/安全/依赖边界变化时更新架构；Markdown 不完整复制 schema |
| 前端页面、状态或查询变化 | 前端实现文档和对应源码/测试 | 分层或鉴权决策变化时更新前端/Management 架构 |
| 工具链、打包、运行或发布脚本变化 | 实际脚本/manifest 与 implementation/delivery.md | 安装边界、缓存或调用规则变化时更新 rules |
| 只改变协作规则 | 对应 rules 文档和索引 | 影响强制读取路径或权限边界时更新 AGENTS；skill 只更新必要路由 |
| 文档迁移/删除 | 最近一级索引、全部入站链接、源码引用 | 一级分类变化时更新 docs/README 和 AGENTS；清理无职责旧目录 |

“实时维护”定义为：有文档影响的代码变更在同一交付批次中完成相应文档更新；合并/交付前解释未更新文档的原因。不能只要求定期刷新日期，更不能用定时生成摘要代替源码核对。

设计已批准、实现尚未落地时，设计可先行，但要标明差距；实际实现完成时更新 implementation 和 plan，而不是继续向架构和根 README 追加“本轮完成”。

### 7.3 规则、AGENTS 和 skill 分工

- `rules/documentation-maintenance.md` 是文档维护规则的唯一完整定义：分类、模板、状态、触发矩阵、退出和验证。
- 根 `AGENTS.md` 保留适用于 agent 的强制基线和任务路由，不抄整套文档模板。
- `.agents/skills/project-doc-maintenance/SKILL.md` 保留何时使用、先读什么、如何分类、何时需要同步以及验证步骤；具体状态枚举和完整规则链接到权威文档。
- 各索引只维护本目录入口与职责；新规范新增时更新规则索引和 AGENTS，普通内容变化不机械改全局索引。

这样可以避免目录结构、模板和状态规则在规范、AGENTS、skill 中各维护一份，产生新的漂移。

## 8. 实施顺序与退出条件

### 8.1 已完成：规则层目录迁移

2026-09-05 按用户追加指令，将规则层的索引、文档维护、环境使用和本地测试四份文档迁入 `docs/rules/`。同步调整所有入站路径、目录示意、AGENTS 和项目 skill 的读取入口；没有实施本方案提出的规则逻辑重写，也没有执行架构或实现文档迁移。

### 8.2 待批准：冻结分类和迁移边界

确认三类主体目录、规则层职责、状态模型和第 5 节迁移表。以当时工作树为准复核文档和源码，保留并行产生的无关变更；不把本方案的基线当作未来执行时的最新状态。

退出条件：每份旧文档均有明确去向；每个拟新增文档都能对应真实内容，不存在仅为对称目录而建立的占位文件。

### 8.3 待批准：提取设计和当前实现

先按源码回查区分设计、当前事实、已完成任务和未完成验收，再依第 5 节建立目标文档。优先处理已发现漂移的 README、正式启动/DNS 管线和 Management；其后逐份处理 12 个模块设计、前端和交付说明。

本步骤必须保留唯一且仍有效的约束，不能把无法确认的段落直接删除，也不能原样复制到实现文档。对实质冲突单独列出待决项；不在文档整理中顺带修复产品代码。

退出条件：架构与实现各自拥有清晰职责；关键能力有源码和接线依据；未知边界显式标注。v2 plan 只保留真实未完成项，不丢失有效验收要求。

### 8.4 待批准：原子更新导航和维护入口

在同一迁移交付中更新 `docs/README.md`、各分类索引、根/前端 README、AGENTS、规则正文和项目 skill；重新计算所有相对链接。目标职责到位后删除旧 `docs/backend/`、`docs/frontend/` 的已迁移文件和空目录，不保留长期平行版本。

迁移中允许尚未提交的临时新旧文件共存，但交付状态必须只有一个权威入口。依赖内容拆分的强制路由不能提前指向不存在或未核验的文档。

退出条件：任何读者都可以从总索引进入三类文档；代码任务必读路由同时覆盖相关设计与当前实现；旧路径没有未处理的有效引用。

### 8.5 待批准：轻量验证与方案退出

先执行第 9 节的人工和结构检查。建议在结构稳定后补充一个不依赖新增工具的本地只读检查入口 `script/check-docs.ps1`，用于重复执行链接、目录和格式检查；脚本不得自动修正文档或更新时间。

检查器的支持范围应限定并说明仓库实际使用的 Markdown 语法，使用小型固定夹具覆盖代码块/行内代码、相对路径、锚点和重复标题等情况；未支持的语法应明确报告，不能默认为检查成功。它不判断代码是否真的实现，也不通过关键词自动重写文档。

当前 `.github/workflows/release.yml` 面向 tag 发布，不应为文档整理触发构建或发布。是否把检查器接入独立 PR 门禁另行决定；本方案不要求新增 CI workflow、站点生成器、外部服务或定时任务。

退出条件：结构迁移及规则调整通过验证；本方案的长期规则已进入 `rules/documentation-maintenance.md`，无需继续保留的调查和步骤交由 Git 历史追溯，再删除本方案及索引项。v2 plan 是否完成独立判断，不随本方案一起自动删除。

## 9. 验证与验收

### 9.1 结构和链接

- 目标目录与索引一致，每个多文档目录有 `README.md`，没有空目录、备份区或跳转副本。
- 检查所有受跟踪 Markdown 的本地文件链接与锚点，包括根/前端 README、AGENTS、项目 skill；排除代码示例和外部 URL，示例不能被误判为失效链接。
- 通过 `rg` 搜索旧目录和文件路径；迁移方案中明确标注的历史路径不视为活动入口，但其余文档不能继续引用已删除文件。
- 重命名后重新计算相对路径，不使用简单替换假设所有文档层级不变。
- 检查 UTF-8 无 BOM、LF、标题层级、表格和 fenced code block，再运行 `git diff --check` 与 `git status --short`。

### 9.2 内容与源码

| 验收场景 | 合格标准 |
| --- | --- |
| 判断能力是否存在 | 从 implementation 找到代码、正式入口接线和验证边界，不需要翻 plan 或 Git 历史 |
| 理解设计取舍 | 从 architecture 找到职责、约束和取舍，不夹杂历史测试次数和逐阶段任务 |
| 启动一个需求 | plan 说明相对当前基线的变化、步骤和验收，不重新复制全套架构 |
| 新增字段或 API | 原始 schema/代码、实现说明和测试同步；生成类型不是人工权威 |
| 查找维护规则 | 从 rules 或 AGENTS 路由定位唯一规范；skill 只负责执行导航 |
| 区分实现与验证 | “代码存在”“已接入正式路径”“某环境验证通过”分别有证据，不能相互推导 |
| 完成一个任务 | 更新实现事实，必要时更新设计，关闭并清理 plan；不永久保留一套完成进度文档 |

纯文档迁移不要求重跑前后端全量测试。若迁移过程中要新增运行结论，必须实际执行对应验证；若发现必须修改代码、schema、配置或发布脚本才能消除冲突，先明确新范围并取得授权。

### 9.3 本轮核验边界

已做文档目录/路由盘点和第 2 节所列代码静态追踪；完成的实际变更仅为规则目录迁移、引用同步和本方案产物。没有将当前未核对模块的全部陈述认定为事实，也没有复用历史测试数量宣称本轮通过。

未运行 Cargo、pnpm、服务 smoke 或发布命令。当前 shell 提示项目声明的 `rust@1.98.0` 缺失；本轮只读分析和文档维护不依赖 Rust，不为此安装工具。后续需要运行代码验证时，按环境规则检查实际工具链。

## 10. 风险与控制

| 风险 | 控制方式 |
| --- | --- |
| 目录换名后内容仍混杂 | 按段落职责拆分，并用入口、契约、证据三项复核；不能仅靠标题分类 |
| 新设计/实现文档形成两套重复权威 | 设计记录不变量和取舍，实现记录接线及限制；完整字段留在 schema/唯一配置参考 |
| 误删唯一约束或尚未完成的验收 | 先完成迁移表和逐项来源核对；v2 plan 的验收单独确认，不能随目录整理关闭 |
| 日期更新造成“实时”假象 | 核对日期绑定源码范围和基线，纯迁移不刷新已存在事实的验证日期 |
| 将源码中的缺陷合法化为新契约 | 明确区分实际行为和已接受设计，实质冲突单独决策，不顺带改代码 |
| 文档数量和维护成本膨胀 | 保留有独立价值的模块设计；实现按实际链路组织，不自动创建每文件/每模块配对文档 |
| 检查器让人误以为语义已验证 | 自动检查仅证明支持语法范围内的结构正确；源码和运行证据仍由人工核对 |

本方案不新增产品功能，不改源码目录、API schema 位置、配置语义、工具版本或发布行为。除已明确授权的 `docs/rules/` 迁移外，其余实施以本方案获批为前提；提交和推送仍需另行授权。
