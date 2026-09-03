# FluxDNS 前端开发方案

> 文档状态：有效
>
> 实现状态：已实现（真实浏览器与双平台发布验收待环境执行）
>
> 适用范围：FluxDNS 只读 WebUI 首版从契约冻结、工程初始化到可交付验收的具体开发步骤；不负责配置写入、运行时控制或 DNS 数据面功能
>
> 最后核对：2026-09-04
>
> 关联文档：[前端架构设计](architecture.md) · [前端工程入口](../../frontend/README.md) · [后端架构设计](../backend/architecture.md) · [后端开发计划](../backend/development-plan.md)

## 1. 结论

当前前端已完成 F0–F5 的代码实现：工程骨架、setup 初始化、鉴权、应用壳和只读页面已接入后端 Management API；类型检查、28 项组件/contract tests、生产构建和 `webui-embed` 静态资源测试已通过。真实浏览器同源 Network/Storage smoke 与双平台发布仍需在对应环境执行，因此文档不把 mock 或 handler 测试描述为真实端到端证据。

首版交付目标是一个通过同源 `/api/v1` 访问管理面的 React + TypeScript + Vite SPA，使用服务端 session Cookie，提供登录、Dashboard、Runtime、Health、Statistics、Queries、Resources 和 System 的只读视图。首版不把任何前端页面或请求接到 UDP/TCP/DoH 数据面，不读取 SQLite、配置文件或服务日志，也不新增配置编辑、reload/restart、缓存清理、资源刷新和 WebSocket/SSE。

本方案确定前端实现顺序、模块边界、接口依赖和验收门槛；Management API 的具体 JSON 字段、认证策略和静态文件托管目标已收敛到 [`frontend/openapi/management-api-v1.yaml`](../../frontend/openapi/management-api-v1.yaml) 和[前端架构设计](architecture.md)。前端通过稳定的同源 API client 消费后端契约，不复制后端配置、存储或 DNS 数据面逻辑。

### 1.1 当前实施进度

| 阶段 | 状态 | 当前证据与边界 |
| --- | --- | --- |
| F0 | 已完成 | Node.js 26.8.1、pnpm 11.25.0、OpenAPI v1、同源 session、setup 和静态托管契约已冻结。 |
| F1 | 已完成 | React/TypeScript/Vite、strict typecheck、路径别名、`/api` proxy、QueryClient、错误边界和路由级代码拆分已实现。 |
| F2 | 已完成 | 统一 fetch、超时/取消、错误 envelope、401、session/login/logout 和 `ProtectedRoute` 已实现并有测试。 |
| F3 | 已完成 | 应用壳、Dashboard、Runtime、Health、System 与受控轮询已实现。 |
| F4 | 已完成 | Statistics、Queries、Resources 的服务端参数、分页、筛选和安全摘要已实现。 |
| F5 | 已完成代码 | setup -> session、真实 API client、静态托管契约和发布入口已接入；真实浏览器安全 smoke 与双平台 binary 仍待相应环境。 |

## 2. 当前基线和事实依据

| 对象 | 已核对事实 | 对本方案的影响 |
| --- | --- | --- |
| `frontend/` 工程 | [前端工程入口](../../frontend/README.md) 维护已实现的 package manifest、构建/测试命令、OpenAPI 类型生成和 mock 预览入口。 | F1–F4 已完成；后续改动必须复用现有工程和模块边界。 |
| WebUI 配置 | `WebUiDto`/`ResolvedWebUi` 保留 `enable`、监听地址、端口、origin 和用户 hash；`enable=true` 已启动独立 Management Server。 | 前端通过 setup -> session 顺序消费服务端状态；不得绕过 management API 读取配置或数据库。 |
| Runtime 摘要 | `RuntimeSnapshot::summary()` 已提供 revision、normalized hash、listener/bind/resource 数量和 `has_policy_core`；`BindEntry` 还包含 transport、地址、端口、owner 和 `v6_only`。 | Runtime 页面应消费后端安全 DTO，不直接序列化 `ResolvedConfig` 或 socket 对象。 |
| Resource 摘要 | `ResourceRegistrySnapshot::summary()` 只提供资源版本、来源类型、fallback 和 stale 状态；资源正文和编译结果不属于管理面。 | Resources 页面只显示元数据、版本和状态，不下载、解析或展示规则正文。 |
| Storage 统计/详情 | 统计按 UTC 日和有限维度聚合；`resolve_log` 含解析详情和敏感字段，Storage 规定详情与统计分别受有界 writer/上限保护。 | Statistics 使用服务端聚合和分页查询；Queries 必须先定义脱敏 projection，不能把 SQLite 行原样返回浏览器。 |
| Telemetry/Health | Telemetry 组件和 health 状态有固定枚举（`healthy`、`degraded`、`failed`、`stopping`），并记录时间、重试、stale/gap 等状态信息。 | Health 页面展示稳定状态和安全原因分类；不展示原始错误堆栈、SecretRef、原始 IP、完整 query 或 header。 |
| Management API | 后端已实现 setup、login/logout/session、七个只读查询、请求边界、错误 envelope 与静态资源 fallback；前端 API client 复用 OpenAPI 生成类型。 | setup、session 和只读页面可走同源 `/api/v1`；真实浏览器 smoke 仍需单独记录。 |

核对依据包括 [`WebUiDto`](../../backend/src/config/model.rs)、[`validate_config`](../../backend/src/config/validate.rs)、[`RuntimeSnapshot`](../../backend/src/runtime/snapshot.rs)、[`ResourceSnapshot`](../../backend/src/resource/snapshot.rs)、[`Storage` 契约](../../backend/src/ports/storage.rs)、[`Telemetry` 契约](../../backend/src/ports/telemetry.rs) 和 SQLite migrations。具体跨模块边界仍以前端架构和后端架构为权威。

## 3. 目标、非目标和交付边界

### 3.1 首版目标

- 初始化独立的 `frontend/` SPA 工程，依赖、构建物和测试产物不越过前端目录边界。
- 建立统一 HTTP client、错误 envelope、查询缓存、session 状态和受保护路由。
- 完成首版只读页面：登录、Dashboard、Runtime、Health、Statistics、Queries、Resources、System。
- 完成首次初始化页面：空用户配置进入 `/initialize`，成功后自动创建 session 并进入 Dashboard。
- 所有查询支持加载、空数据、服务端错误、超时、取消和 session 过期状态；分页和过滤由后端约束。
- 生产构建输出 `frontend/dist`，静态路由回退到 `index.html`，`/api/*` 始终分流到 management API。
- 以 API fixture/contract test 支撑前端独立开发；Management API 已就绪，真实同源浏览器集成验证作为环境验收单独记录。

### 3.2 首版明确不做

- 配置 YAML 的查看、编辑、校验、迁移、保存或热加载。
- `reload`、`restart`、rebind、缓存清理、资源手动刷新和其他 runtime command。
- 浏览器直接发送 DNS 查询、调用 DoH endpoint 或承担 DNS wire 编解码。
- 读取本地 SQLite、缓存数据库、配置文件、日志文件或 SecretRef 实际值。
- WebSocket/SSE、实时告警推送、复杂报表导出和离线缓存。
- 前端自行实现授权、限流、CSRF 防护或把隐藏路由当作权限控制。

这些能力即使在后端后续实现，也应先形成独立的权限、审计、并发 revision 和失败回滚契约，再扩展前端模块。

## 4. 总体方案

```text
Browser
  └─ React SPA
      ├─ Router / ProtectedRoute
      ├─ QueryClient（缓存、分页、轮询、取消）
      ├─ Auth session（仅内存状态，凭服务端 Cookie）
      └─ feature modules
          └─ same-origin /api/v1/*
              └─ Management API adapter
                  ├─ RuntimeSnapshot / BindPlan 摘要
                  ├─ Storage 统计与脱敏详情 projection
                  ├─ ResourceSummary
                  └─ Telemetry/Health snapshot
```

开发环境由 Vite 提供静态资源，并将 `/api` 代理到本地 management API；生产环境只发布 `vite build` 产物，由 management server 提供静态文件和同源 API。前端 API client 固定使用相对路径，生产环境不开放任意 `baseURL` 或跨域目标。

页面只消费后端管理面 DTO。后端 adapter 负责从 `RuntimeCoordinator`、`StorageRuntime`、`ObservabilityRegistry` 等内部边界构造稳定的 JSON read model；前端不把 Rust 内部类型、配置继承规则或数据库 schema 直接映射成业务状态。

## 5. 管理 API 前置契约（阶段 F0）

### 5.1 接口分组

下表汇总已由 [`management-api-v1.yaml`](../../frontend/openapi/management-api-v1.yaml) 冻结的前端目标契约；后端接口仍未实现：

| 分组 | 目标接口 | 前端用途 | 当前状态 |
| --- | --- | --- | --- |
| Session | `POST /api/v1/auth/login`、`POST /api/v1/auth/logout`、`GET /api/v1/auth/session` | 登录、登出、启动时恢复会话 | 前端 schema/fixture 已实现；后端未实现 |
| 总览 | `GET /api/v1/overview` | Dashboard 卡片和采样时间 | 前端 schema/fixture 已实现；后端未实现 |
| Runtime | `GET /api/v1/runtime` | revision、listener/bind 和运行时摘要 | 前端 schema/fixture 已实现；后端未实现 |
| Health | `GET /api/v1/health` | 组件状态、原因分类和更新时间 | 前端 schema/fixture 已实现；后端未实现 |
| Statistics | `GET /api/v1/statistics` | 时间范围、维度聚合和分页/限制 | 前端 schema/fixture 已实现；后端未实现 |
| Queries | `GET /api/v1/queries` | 脱敏解析详情的服务端分页 | 前端 schema/fixture 已实现；后端未实现 |
| Resources | `GET /api/v1/resources` | 资源版本、来源、fallback、stale | 前端 schema/fixture 已实现；后端未实现 |
| System | `GET /api/v1/system` | 版本、运行时间和只读能力摘要 | 前端 schema/fixture 已实现；后端未实现 |

除 login/logout 外，首版只允许 `GET` 查询。若后端最终采用不同路径或版本号，必须在 F0 更新本表和前端类型，不在页面中保留兼容猜测分支。

### 5.2 Read model 来源和安全投影

| Read model | 允许的来源 | 首版前端可展示 | 禁止直接暴露 |
| --- | --- | --- | --- |
| Overview | Storage 聚合统计 + Runtime/Health 摘要 | 有界计数、状态、采样时间、runtime revision | 原始事件、完整 qname、请求/客户端明文 |
| Runtime | `RuntimeSnapshotSummary` + `BindEntry` | revision、hash 摘要、listener/bind 数量、transport、地址/端口、draining 状态（若契约提供） | socket handle、`ResolvedConfig` 全量、证书和私钥 |
| Health | Telemetry/Storage/Runtime health snapshot | component、状态、first/last changed、last success、retry、stale/gap、稳定原因码 | 原始错误、SecretRef、完整内部路径 |
| Statistics | `stats_daily_total`、`stats_daily_dimension` 的服务端聚合 | UTC 日期、total、有限 dimension kind/value、count | 任意域名、完整 client ID、原始 IP、未受限 group-by |
| Queries | `resolve_log` 的后端安全 projection | 时间、耗时、transport、source、rcode、cache/outcome、策略/资源是否命中等摘要 | `canonical_qname`、request digest、原始 client/route 文本、DNS wire |
| Resources | `ResourceSummary` 和受控 metadata | ID 的安全展示名、epoch/revision、source kind、fallback、stale | 规则正文、hosts 内容、远程 URL 凭据、编译对象 |
| System | 后端显式提供的版本/能力 read model | 服务版本、运行时间、只读能力列表 | 配置文件路径、环境变量、凭据和内部诊断详情 |

`resolve_log` 当前 schema 含 `canonical_qname`，但这不等于允许 management API 返回该字段。Queries 的字段投影、权限和保留期限必须在 F0 由后端安全契约明确；在契约冻结前，前端按“不展示原始 qname 和请求标识”实现。

### 5.3 通用响应和错误契约

- 响应使用 JSON；时间统一为 UTC RFC 3339 字符串；revision、epoch、计数和 opaque ID 按契约声明的类型处理，不从字符串猜测业务含义。
- 错误使用包含 `code`、用户可读 `message`、`request_id`、`retryable` 的稳定 envelope；前端按 HTTP status 和 `code` 分类，不按自然语言匹配。
- `401` 清理内存 session 并跳转 `/login`；`403`、`429`、`5xx`、网络错误和超时分别展示权限、限流、服务端和连接失败状态。
- 列表接口必须由服务端施加最大页大小、最大时间范围、允许的过滤字段和排序字段；前端控件只能选择契约声明的选项。
- 变更 session 的请求遵循后端 CSRF 契约；前端不自行添加与后端不一致的 token/header 方案。
- API 不得返回密码、`password_hash`、SecretRef 实际值、代理凭据、TLS material、原始 header、完整 IP 或未脱敏错误堆栈。

### 5.4 F0 退出条件

F0 未通过前不进入真实页面联调。退出条件为：

1. 后端确认独立 management router/adapter 的生命周期、绑定方式和同源静态文件边界，不改写 DoH handler 语义；
2. 每个接口的请求参数、响应 DTO、错误码、分页/时间限制、认证/CSRF 和脱敏字段有可审查的 schema 或 Rust contract test；
3. 明确 `Queries` 是否允许任何 qname 展示；默认选择安全摘要；
4. Node.js 26.8.1、pnpm 11.25.0、`frontend/pnpm-lock.yaml`、Vitest/Testing Library/MSW 已成为仓库基线，缓存固定在 `frontend/.cache/`；
5. 生产发布由未来的 management server 托管通过 `webui-embed` 编译期内嵌到单个 Rust binary 的前端资源；`frontend/dist` 仍作为独立构建物保留，未知 SPA 路由回退 `index.html`，`/api/*` 始终交给 API router。

## 6. 前端目录和模块

阶段 F1 之后的建议目录如下，具体文件随实现增量创建，不预建空模块：

```text
frontend/
├── README.md
├── package.json                 # F1：依赖和脚本
├── <lockfile>                  # F1：由选定包管理器生成
├── tsconfig.json
├── vite.config.ts
├── public/
└── src/
    ├── app/
    │   ├── App.tsx
    │   ├── providers.tsx        # Router、QueryClient、Auth
    │   ├── router.tsx
    │   └── query-client.ts
    ├── modules/
    │   ├── auth/               # login/logout/session
    │   ├── dashboard/          # overview
    │   ├── runtime/            # runtime/bind 摘要
    │   ├── health/             # 组件健康
    │   ├── statistics/         # 时间范围和维度聚合
    │   ├── queries/            # 服务端分页详情
    │   ├── resources/          # 资源 metadata
    │   └── system/             # 版本和能力
    └── shared/
        ├── api/                # fetch、错误、DTO 边界
        ├── components/         # 无业务 UI
        ├── formatters/         # 时间、状态、数量
        ├── hooks/
        └── styles/
```

`modules/*` 只能通过自己的 `api.ts`、query hook、page 和 view model 访问管理 API；模块之间不得导入对方内部实现。查询数据只由 TanStack Query 管理，Context 仅承载 session 和少量布局状态，不建立第二份全局数据 store。HTTP、错误、格式化和通用状态组件进入 `shared/`，其中不得放入 DNS 业务规则。

## 7. 分阶段实施方案

### F0：契约和工程决策（前置阶段）

| 项目 | 内容 |
| --- | --- |
| 主要范围 | 完成 5.1–5.4 的 management API read model、错误/认证、安全和静态托管决策；确认 Node/包管理器/测试工具。 |
| 主要产物 | 可审查的 API schema 或 Rust contract test、脱敏字段清单、前端依赖白名单和同源部署约定。 |
| 依赖 | 后端 management API 设计；不要求此阶段启用 `webui`。 |
| 退出条件 | 所有拟议接口从“建议”变为有版本、有字段、有错误码的契约；未决项归零或显式延期。 |

### F1：工程初始化和应用基础设施

| 项目 | 内容 |
| --- | --- |
| 主要文件 | `frontend/package.json`、锁文件、`tsconfig.json`、`vite.config.ts`、`src/app/*`、`src/shared/api/*`。 |
| 主要实现 | React + TypeScript + Vite 工程、TypeScript strict、路径别名、`/api` dev proxy、生产相对路径、QueryClient、基础样式和错误边界。 |
| 约束 | 构建物只写入 `frontend/dist`；依赖和缓存按[项目环境使用规范](../standards/environment-usage.md)放置并由 `.gitignore` 覆盖；不把环境变量变成任意生产 API 地址。 |
| 验证 | 类型检查、`vite build`、路由加载和浏览器 fixture smoke 已通过，证据见 10.1。 |
| 退出条件 | 空壳可启动，`/login`、受保护路由和 404 路由可渲染，开发代理和生产相对 API 路径配置可审查。 |

### F2：统一 API client 和 session

| 项目 | 内容 |
| --- | --- |
| 主要文件 | `src/shared/api/client.ts`、`errors.ts`、`types.ts`、`src/modules/auth/api.ts`、`hooks.ts`、`pages/*`、`src/app/router.tsx`。 |
| 主要实现 | `fetch` 封装、`credentials: same-origin`、超时和 `AbortController`、JSON 解码、错误 envelope、`401` 统一处理、session query、login/logout mutation、`ProtectedRoute`。 |
| 安全要求 | session 只保留在内存；不写入 localStorage/IndexedDB/URL/日志；密码仅存在提交表单的瞬时内存，不进入 query cache 或错误信息。服务端 Cookie 属性由后端负责。 |
| 验证 | fake transport 下覆盖成功、非 JSON 错误、401/403/429/5xx、超时、取消、登出清理和路由回跳。 |
| 退出条件 | 无有效 session 时所有受保护路由均不可见；session 过期后只保留一次跳转和可读错误提示；请求取消不会更新已卸载页面。 |

### F3：应用壳与核心只读页

| 项目 | 内容 |
| --- | --- |
| 主要文件 | `src/modules/dashboard/*`、`runtime/*`、`health/*`、`system/*`、`src/shared/components/*`。 |
| 主要实现 | 侧栏和面包屑、统一页面布局、加载/空态/错误态、状态标签、表格基础组件；接入 overview/runtime/health/system query。 |
| 页面边界 | 只展示后端 DTO 的摘要；Runtime 不提供 command 按钮，Health 不提供修复动作，System 不展示敏感配置路径。 |
| 刷新策略 | Dashboard、Runtime、Health 使用受控轮询；页面隐藏时暂停非必要刷新；System 默认按进入页面查询。具体间隔以 API 限流契约为准。 |
| 验证 | query key 稳定性、轮询暂停/恢复、组件状态快照或 Testing Library 测试、路由守卫和错误重试按钮。 |
| 退出条件 | 首屏可在无数据、部分组件失败和整页失败时清晰区分状态；不因单个组件故障伪造整体成功。 |

### F4：统计、解析详情和资源页

| 项目 | 内容 |
| --- | --- |
| 主要文件 | `src/modules/statistics/*`、`queries/*`、`resources/*`、`src/shared/formatters/*`。 |
| 主要实现 | Statistics 的时间范围/维度选择和聚合展示；Queries 的服务端分页、有限过滤、排序、详情摘要；Resources 的版本、来源、fallback、stale 状态。 |
| 数据边界 | 前端不全量加载解析记录，不在浏览器端聚合统计，不下载规则正文，不自行推导资源/策略语义；所有筛选和排序参数来自契约枚举。 |
| 一致性 | query key 包含全部服务端参数；参数变化取消旧请求并重新查询；响应携带采样时间和 runtime revision 时在页面展示，避免把不同采样误拼成一个快照。 |
| 验证 | 分页边界、空结果、非法参数被服务端拒绝、重复点击/快速切换取消、脱敏字段断言和确定性格式化测试。 |
| 退出条件 | 三个页面均能在 mock contract 下独立运行；真实 API 只需替换 transport，不需改动页面业务逻辑。 |

### F5：同源集成、交付和回归

| 项目 | 内容 |
| --- | --- |
| 主要范围 | 与后端 management API 的最小垂直切片联调、静态托管、生产错误分流和浏览器 smoke。 |
| 集成路径 | 使用本地 `_fluxdns/` 配置启动 Management Server；前端通过同源 `/api/v1` 验证 setup/login/session、一个健康接口、一个分页接口和静态路由回退。 |
| 验证 | `typecheck`、组件/contract tests、`vite build`、后端静态资源 fallback/HEAD/ETag、`/api` 不被回退为 HTML、setup/401 session 过期、Cookie 不落地、浏览器网络面无 DNS wire/SecretRef。 |
| 退出条件 | 关键路径有可复现记录，所有未实现或未验证项单独列出；前端构建物、依赖、测试缓存和本地配置不进入 Git。 |

## 8. 页面和状态要求

| 页面 | 主数据 | 必须处理的状态 | 首版交互边界 |
| --- | --- | --- | --- |
| `/initialize` | setup status + initialize | 首次加载、密码校验、并发 409、网络失败 | 仅创建首个用户；不保存密码或 token |
| `/login` | session login | 首次加载、认证失败、限流、网络失败 | 仅登录；不注册、不改密码、不保存 token |
| `/dashboard` | overview | 局部卡片失败、采样时间、轮询暂停 | 只读摘要和跳转入口 |
| `/runtime` | runtime/bind | 无 listener、draining、revision 变化 | 只读 listener/bind 信息 |
| `/health` | component health | `healthy/degraded/failed/stopping`、stale/gap | 展示安全原因码；不提供修复动作 |
| `/statistics` | daily total/dimension | 时间范围超限、空结果、聚合失败 | 服务端聚合；不导出、不自定义任意维度 |
| `/queries` | paginated safe projection | 无详情、页边界、筛选/排序失败 | 不展示原始 qname、request digest 或 wire |
| `/resources` | resource summary | stale、fallback、资源缺失 | 不刷新、不下载正文 |
| `/system` | version/capability | 能力缺失、版本未知 | 只读版本和能力摘要 |

统一页面状态顺序为：`loading → success/empty → retryable error/non-retryable error`。错误提示使用后端 `code` 的本地化映射；未知 code 使用通用安全提示，不把后端自然语言或堆栈直接渲染为 HTML。

## 9. 安全、性能和失败语义

### 9.1 安全

- 使用服务端 `HttpOnly` Cookie；前端不实现长期 token 存储和刷新协议。
- 生产使用同源相对路径；任何跨域需求必须先更新后端 CORS、CSRF、Cookie 和部署契约。
- 所有后端字符串按纯文本渲染；不使用 `dangerouslySetInnerHTML` 展示状态、错误、URL 或日志字段。
- 不把完整 `canonical_qname`、request digest、client IP、ECS、SecretRef、password hash、代理凭据、TLS material 或 raw DNS wire 放入 UI state、日志、URL 或浏览器存储。
- 服务端授权是最终边界；前端隐藏菜单只改善体验，不能替代 `401/403` 响应处理。

### 9.2 性能和一致性

- 仅使用 TanStack Query 管理 server state，query key 必须包含查询参数和 API 版本。
- 所有请求都绑定 `AbortController`；路由离开、筛选变化和组件卸载时取消不再需要的请求。
- Statistics 和 Queries 由服务端分页/聚合；前端不加载无上限集合。
- 轮询只用于 Dashboard/Runtime/Health 等明确需要的摘要，并在页面不可见时暂停；出现 `429` 时按 retryable 语义退避。
- 页面展示服务端采样时间和 runtime revision（若契约提供），不把多个响应在客户端强行合并为一致快照。

### 9.3 失败处理

网络、超时、取消、`401`、`403`、`429`、`5xx` 和非 JSON 响应必须保持可区分。失败不能被转换成空数组或“服务正常”；局部组件可以独立降级，但整个页面必须保留失败标识和重试入口。后端 health 的 `degraded`/`failed` 是服务状态，不等同于前端请求失败，页面应分别展示。

## 10. 验收和证据口径

### 10.1 前端静态/组件验收

F1 工具链确认后，从 `frontend/` 实际执行：

```text
pnpm install --frozen-lockfile
pnpm run typecheck
pnpm run test
pnpm run build
```

2026-09-04 的当前证据为：`typecheck` 通过；Vitest 5 个 test files、28 项 tests 通过；Vite 生产构建成功，生产 `dist/` 不包含 MSW worker；后端 `webui-embed` 测试覆盖 SPA fallback、HEAD、ETag 和静态响应头。MSW fixture smoke 覆盖 setup、初始化、登录、只读页面和登出，并断言未写入浏览器持久化存储；Windows `webui-embed` release binary 的真实 HTTP smoke 已覆盖 SPA 200、setup required、未认证 401、初始化后 session/overview 200 和重复 setup 409，且在临时移出 `frontend/dist/` 后仍能返回嵌入 SPA；in-app browser 已检查真实 `/initialize` 页面、表单校验和 Console。浏览器 Network/Storage 观察与 Linux target 发布尚未执行，不将这些边界写成已通过。

### 10.2 Management API 集成验收

后端接口可用后，至少完成：

1. 登录、session 恢复、登出和 `401` 过期跳转；
2. overview/runtime/health 中至少一条真实查询，确认 JSON content type、request ID 和采样时间；
3. statistics 的时间范围/维度限制，以及 queries 的服务端分页和脱敏 projection；
4. resources 的 resource version/fallback/stale 与 Runtime snapshot 一致；
5. 同源静态路由回退、`/api` 分流和失败状态；
6. 浏览器 Network、Storage 和 Console 检查无 token、密码、SecretRef、raw wire 或未脱敏错误。

### 10.3 文档和工作树验收

- 更新 [`docs/frontend/README.md`](README.md) 的文档路由；
- 新文档只使用仓库内相对链接，不写个人绝对路径、凭据或本地产物；
- 执行 `git diff --check`、链接目标检查和 `git status --short`；
- 若后续实现改变 API、配置或静态托管契约，同步更新前端架构、后端架构/配置文档和相关测试说明；
- 未实施部分必须保持明确边界：后端 Management API 已实现，但真实浏览器同源联调与双平台发布验收仍不能以 mock、handler 或单平台构建替代。

## 11. 风险、决策门和后续拆分

| 风险/未决项 | 影响 | 处理方式 |
| --- | --- | --- |
| 真实浏览器与双平台环境尚未执行 | 无法把 Cookie/CSP/静态托管和发布 binary 标记为最终验收通过 | 在具备本地 Management Server、浏览器和对应 Rust target/linker 的环境补做；保留当前 mock、handler 和 embed 测试作为代码级证据。 |
| 项目级 Node/pnpm 基线 | 已解决；工具版本与缓存路径可复现 | `mise.toml` 固定 Node.js 26.8.1 / pnpm 11.25.0，前端 manifest、lockfile 与环境规范同步维护。 |
| `resolve_log` 含敏感 qname | 错误 projection 可能泄露请求数据 | 默认只做安全摘要；后端 contract test 固定禁止字段，任何扩展需单独授权。 |
| health/metrics 只有内部 registry | 页面可能直接依赖实现细节 | 后端增加只读 adapter/DTO；前端只依赖版本化 JSON。 |
| 轮询增加管理面负载 | 多页面打开时触发限流 | 由 API 契约给出采样/限流边界；页面隐藏暂停，`429` 退避，避免各模块自定义无限重试。 |
| 发布环境 target/linker 不完整 | 无法生成双平台单 binary | `package-embedded.ps1` 在构建前检查 feature/target，缺失时 fail fast；不自动安装工具链。 |

后续若增加配置写入或 runtime command，应另建独立方案，至少补充权限模型、CSRF、审计、revision conflict、幂等、确认交互和失败回滚；不得在本方案的只读模块中预留未定义的 command client。

## 12. 参考资料

- [前端架构设计](architecture.md)
- [后端架构设计](../backend/architecture.md)
- [后端开发计划](../backend/development-plan.md)
- [Storage 模块设计](../backend/modules/storage.md)
- [Observability 模块设计](../backend/modules/observability.md)
- [项目环境使用规范](../standards/environment-usage.md)
- [本地测试规范](../standards/local-testing.md)
- [文档维护规范](../standards/documentation-maintenance.md)
