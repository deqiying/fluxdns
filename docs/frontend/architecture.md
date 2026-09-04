# FluxDNS 前端架构设计

> 文档状态：有效
>
> 实现状态：已实现
>
> 适用范围：FluxDNS 只读 WebUI 第一阶段的总体架构、技术栈、Management API 边界与实施顺序
>
> 最后核对：2026-09-04
>
> 关联文档：[前端工程入口](../../frontend/README.md) · [后端架构设计](../backend/architecture.md)

## 1. 结论

FluxDNS 前端已实现 **React + TypeScript + Vite 的独立 SPA**，并已接入后端 Management Server 的 setup、登录、session、登出和只读查询 API。首次访问按 `setup -> session` 顺序判定，空用户配置进入 `/initialize`，完成初始化后使用服务端签发的 Cookie session；生产发布由 Management Server 将 `frontend/dist` 编译期内嵌到单个 Rust binary，浏览器通过同源 `/api/v1` 访问后端 JSON API。当前代码级构建、测试、`webui-embed` 静态资源验收和 Windows 当前平台打包已通过；三平台自动发布流程已配置，真实浏览器同源 Network/Storage smoke、GitHub Actions 发布与 Linux/macOS 原生发布仍需在对应环境执行。

当前阶段的 WebUI 只负责：

- 登录、登出和当前会话查询；
- Dashboard、运行状态、健康状态和版本信息查询；
- 解析统计、解析记录和资源状态查询。

当前阶段明确不负责：

- DNS 查询代理、DoH 转发或上游测试解析；
- 配置编辑、配置写入和配置迁移；
- `reload`、`restart`、缓存清理、资源刷新等运行时控制；
- WebSocket/SSE 实时推送；首版使用有界轮询。

配置写入和运行时控制作为后续独立能力设计，不在当前前端中预留可调用的 command API、控制页面或权限分支。

## 2. 现有约束

### 2.1 仓库边界

`frontend/` 是独立的前端主目录，已包含 package manifest、pnpm lockfile、Vite/Vitest 配置、OpenAPI schema 和源码。前端实现不得把仓库根目录作为工程目录，也不得把构建物、依赖或 `.cache/` 提交到仓库。相关构建物、依赖和缓存目录约定见[项目环境使用规范](../standards/environment-usage.md)，本地测试运行时文件见[本地测试规范](../standards/local-testing.md)。

### 2.2 后端边界

FluxDNS 后端是单 Rust binary。DoH 入站继续使用独立的 HTTP/1.x parser；Management Server 使用独立的 `axum` HTTP/1.1 listener、router、认证和静态资源 adapter。`webui.enable: true` 时创建管理 endpoint，`false` 时不创建 management listener、session 或认证状态。[后端架构：WebUI Management Server](../backend/architecture.md#13-webui-management-server) [配置参考：`webui`](../backend/configuration-reference.md#7-webui)

管理 API 与 DoH router 和 DNS handler 保持分离。当前前端只访问 management API，不把现有 DoH 路由当作管理 API，也不承担 DNS 数据面的协议转换。

### 2.3 数据来源

只读页面的数据由后端已有的稳定边界提供：

- `Storage`：解析统计、聚合结果和有界解析记录；
- `Telemetry`/`Health`：服务、listener、数据库和资源健康状态；
- `RuntimeCoordinator`：当前 runtime revision、listener 和资源 snapshot 摘要。

前端不读取 SQLite 文件、不解析后端日志、不接触 `ResolvedConfig` 中的 SecretRef 或密码 hash，也不直接调用 `DnsCore`。

## 3. 总体架构

```text
┌─────────────────────────────────────────────────────────────┐
│ Browser                                                     │
│                                                             │
│ React SPA                                                   │
│  ├─ React Router：页面路由和登录路由守卫                    │
│  ├─ TanStack Query：查询缓存、轮询、分页和请求状态           │
│  ├─ Auth session：只保存当前会话状态，不保存长期 token       │
│  └─ Feature modules：dashboard / statistics / queries ...   │
└──────────────────────────────┬──────────────────────────────┘
                               │ same-origin /api/v1/*
                               ▼
┌─────────────────────────────────────────────────────────────┐
│ FluxDNS Management API                                      │
│  ├─ Auth：login / logout / session                          │
│  ├─ Read model：overview / runtime / health                 │
│  ├─ Query：statistics / query records / resources            │
│  └─ Static files：SPA entry and hashed assets               │
└──────────────────────────────┬──────────────────────────────┘
                               │ read-only ports
                               ▼
             Storage / Telemetry / Health / RuntimeCoordinator

             DNS Core、UDP/TCP/DoH 数据面不在该调用链中
```

### 3.1 部署模型

- 开发环境由 Vite dev server 提供页面，并将 `/api` 代理到本地 management API；
- 前端独立构建始终执行 `vite build`，生成并保留 `frontend/dist`；
- 生产环境优先使用同源路径提供静态文件和 `/api/v1`，避免在浏览器中配置跨域 API 地址；
- 本地 v2 发布由 `script/package-embedded.ps1` 先构建 `frontend/dist` 与默认 feature 的后端 release，再通过 `webui-embed` 将资源编译进当前 Windows/Linux x86_64 平台的 Rust binary；自动 Release workflow 复用共享质量门禁生成的同一份 `frontend/dist`，在 Windows x86_64、Linux x86_64 和 macOS ARM64 runner 并行构建；
- `backend/target/release` 与 `backend/target/<triple>/release` 继续分别保留后端独立构建物和内嵌 WebUI Cargo 构建物，不与 `deploy/` 混用；
- 生产环境不需要运行 Node.js 服务。

### 3.2 请求方向

当前前端只能发起两类请求：

1. 鉴权请求：登录、登出、会话检查；
2. 查询请求：读取后端已经生成的状态、统计、记录和资源摘要。

不存在浏览器到 DoH endpoint 的管理调用，也不存在通过前端转发 DNS 请求的隐藏路径。

## 4. 技术栈

| 层次 | 选择 | 职责和边界 |
| --- | --- | --- |
| UI runtime | `React` | 函数组件、Hooks 和组件组合；不承担 API 缓存职责。 |
| 语言 | `TypeScript` | 页面、API DTO、查询参数和视图模型的静态类型。 |
| 构建 | `Vite` | 开发 server、HMR 和静态生产构建。 |
| 路由 | `React Router` | 登录页、受保护页面和 404 路由。 |
| Server state | `TanStack Query` for React | 查询缓存、轮询、分页、取消、错误和 stale 状态。 |
| UI 组件 | `Ant Design` | 表格、表单、筛选器、状态标签、分页和反馈组件；主题由 `AppProviders` 统一配置。 |
| HTTP client | 浏览器 `fetch` 封装 | 统一 base path、Cookie、超时、取消、JSON 解码和错误映射。首版不额外引入 Axios。 |
| 客户端全局状态 | React Context/Hooks | 当前仅承载认证会话和少量布局状态；不把查询数据复制到全局 store。 |

当前阶段不默认引入 Redux、Zustand 或其他全局状态库。只有当客户端状态出现明确的跨页面写入需求时，才单独评估；后端查询数据仍由 `TanStack Query` 管理。

API 字段以 [`frontend/openapi/management-api-v1.yaml`](../../frontend/openapi/management-api-v1.yaml) 为当前版本权威，并由 `openapi-typescript` 生成 TypeScript 类型。后端实现必须通过 contract test 对齐该 schema；前端可以在 HTTP 边界增加运行时校验，但不得复制后端配置继承、引用环、策略选择或 DNS 业务校验。

## 5. 前端目录和模块

```text
frontend/
├── README.md
├── package.json
├── pnpm-lock.yaml
├── pnpm-workspace.yaml
├── tsconfig.json
├── vite.config.ts
├── openapi/
├── public/
└── src/
    ├── app/
    │   ├── App.tsx
    │   ├── providers.tsx       # Router、QueryClient、Auth provider
    │   └── query-client.ts
    ├── mocks/                   # MSW contract fixtures
    ├── modules/
    │   ├── auth/
    │   ├── dashboard/
    │   ├── runtime/
    │   ├── health/
    │   ├── statistics/
    │   ├── queries/
    │   ├── resources/
    │   └── system/
    ├── shared/
    │   ├── api/
    │   │   ├── client.ts        # fetch、Cookie、错误和取消
    │   │   ├── errors.ts
    │   │   └── types.ts
    │   ├── components/
    │   └── formatters/
    ├── styles/
    └── test/
```

每个 `modules/*` 模块拥有自己的页面、查询 hook 和 API 映射；测试按模块或跨路由契约放置。模块之间不得互相导入对方的内部 API；可复用的无业务组件、格式化器和 HTTP 基础能力放入 `shared/`。

首版不创建 `config`、`control`、`reload` 或 `cache-management` 模块。后期增加控制能力时，应作为独立模块加入，并通过新的权限和审计契约接入。

## 6. 页面和路由

首版路由如下：

```text
/login
/initialize
/
├── /dashboard
├── /runtime
├── /health
├── /statistics
├── /queries
├── /resources
└── /system
```

- `/login` 和 `/initialize` 不需要已认证 session；
- 应用启动先查询 `/api/v1/auth/setup`，只有 `ready` 状态才继续查询 `/api/v1/auth/session`；
- `required` 状态下受保护路由统一跳转 `/initialize`，初始化成功后自动进入 Dashboard；
- 未认证时只跳转登录页，不在前端猜测或恢复失效 token；
- 认证失败、session 过期或 API 返回 `401` 时清理本地会话并回到 `/login`；
- `403`、`429`、`5xx` 和网络超时分别展示权限、限流、服务端和连接失败状态，不把失败伪装成空数据。

页面职责保持只读：

| 页面 | 读取内容 | 当前不提供 |
| --- | --- | --- |
| Dashboard | 请求量、成功/失败、延迟、缓存和整体健康摘要 | 清理缓存、改变策略 |
| Runtime | active revision、listener、资源 snapshot 摘要 | reload、rebind、restart |
| Health | 组件状态、degraded 原因、更新时间 | 手动修复、强制恢复 |
| Statistics | 时间范围、客户端/策略/上游维度的聚合数据 | 写入统计配置 |
| Queries | 分页解析记录、过滤、排序、详情摘要 | 发起新的 DNS 查询 |
| Resources | hosts/rule-set 来源、版本、hash、刷新状态 | 手动刷新资源 |
| System | FluxDNS 版本、运行时间、能力摘要 | 修改系统配置 |

## 7. Management API 契约

### 7.1 接口分组

首版接口、字段、参数枚举、分页上限和错误响应已在 [`management-api-v1.yaml`](../../frontend/openapi/management-api-v1.yaml) 冻结，后端已按该契约接入；实现和页面均不得猜测兼容分支。

```text
GET  /api/v1/auth/setup
POST /api/v1/auth/setup
POST /api/v1/auth/login
POST /api/v1/auth/logout
GET  /api/v1/auth/session

GET  /api/v1/overview
GET  /api/v1/runtime
GET  /api/v1/health
GET  /api/v1/statistics
GET  /api/v1/queries
GET  /api/v1/resources
GET  /api/v1/system
```

除 `auth/setup` 的一次性初始化写入、`auth/login` 和 `auth/logout` 外，首版 management API 只提供 `GET` 查询接口。当前不创建配置编辑、runtime command 或 DNS query endpoint。

### 7.2 数据契约

- 响应使用 JSON，时间统一为 UTC 的 RFC 3339 字符串；
- ID、revision、resource version 等标识按 opaque string/number 处理，前端不从字符串中推断业务含义；
- 统计和解析记录必须分页，服务端同时施加最大时间范围、页大小和过滤条件限制；
- authenticated 解析记录查询返回 canonical qname、有效 client IP、配置客户端/strategy、target/actual upstream、服务端总耗时、DNS 主链耗时和有界 answer；历史脱敏行以 `legacy_redacted` 显式区分，schema v5 之前的主链耗时为 `null`；
- 响应不得包含密码、`password_hash`、SecretRef 实际值、代理凭据、完整认证头或未脱敏的内部路径；
- 查询响应应带 `request_id` 和必要的 `runtime_revision`，便于页面展示数据采样时间和运行时一致性；
- 资源和健康状态需要携带采样时间、状态和安全的原因分类，不能把后端原始错误堆栈直接返回浏览器。

错误结构固定为稳定的 JSON error envelope，例如：

```json
{
  "code": "AUTH_SESSION_EXPIRED",
  "message": "session expired",
  "request_id": "redacted-request-id",
  "retryable": false
}
```

`message` 面向用户且不包含 secret、原始 YAML、DNS wire 或 SQL 错误。前端根据 `code` 和 HTTP status 决定展示和重试，不根据自然语言匹配错误。

### 7.3 查询缓存和刷新

- 认证 session 查询使用短 stale 时间；登出后主动清理相关 query；
- Dashboard、health 和 runtime 使用短间隔轮询，并在页面隐藏时暂停非必要刷新；
- statistics 使用查询参数形成稳定 query key，支持时间范围和维度变化；
- queries 使用服务端分页，筛选或排序变化时重新查询，不在浏览器端加载全量记录；
- resources 只查询后端已发布的 snapshot/refresh 状态；前端不执行资源下载或解析；
- API 请求都绑定 `AbortController`，路由离开、参数变化或组件卸载时取消过期请求。

首版不依赖 WebSocket/SSE。只有轮询无法满足明确的实时性需求时，才新增带权限边界和断线重连契约的事件接口。

## 8. 鉴权和安全边界

### 8.1 Session

- 使用服务端 session 和 `HttpOnly` Cookie；
- Cookie 设置 `HttpOnly`、`SameSite=Strict` 并限制 Path/Domain；`public_origin` 为 HTTPS 时增加 `Secure`，直接 HTTP 仅用于 loopback/受信管理网络；
- 前端不把长期 token、密码或 `password_hash` 写入 `localStorage`、IndexedDB、URL 或日志；
- 登录和初始化表单只通过同源 management API 提交，登录失败使用统一错误分类；传输安全由 `public_origin` 与部署网络边界负责；
- login 应实施服务端限速和失败计数，前端只展示安全的失败信息。

### 8.2 请求和展示

- 生产环境使用同源 `/api`，不在前端开放任意 `baseURL` 或跨域目标；
- login/logout 由后端校验同源 `Origin` 和 Fetch Metadata，前端不新增自定义 token/header；未来增加写操作时必须另行评审 CSRF 契约；
- 服务端负责最终授权，前端路由隐藏不等于权限控制；
- 表格、日志和错误展示必须对 HTML、URL、header 和日志字段进行文本化处理，不能把后端字符串当作 HTML 注入；
- 查询接口默认最小权限和最小字段，前端不请求未使用的敏感字段。

### 8.3 后端集成约束

后端已提供独立的 `management` adapter/router，并由现有 supervisor 管理其 listener/task 生命周期。管理服务的绑定地址和端口进入全局 TCP bind 冲突校验，但不与 DoH router 共享路由语义。管理 handler 通过只读的 Storage、Telemetry、Health 和 RuntimeCoordinator 入口读取数据，不直接操作 `DnsCore`、socket、SQLite connection 或具体 cache implementation。

现有 `webui.users[].password_hash` 只能在服务端用于密码验证；它不应作为 API 字段返回给前端。[配置参考：用户 hash](../backend/configuration-reference.md#7-webui)

## 9. 构建和交付

### 9.1 开发

开发 server 只负责前端资源和 HMR：

```text
Vite dev server
  ├── /                → React SPA
  └── /api/*           → proxy 到本地 FluxDNS management API
```

API client 使用相对路径，避免开发环境代码与生产环境绑定不同域名。

### 9.2 生产

```text
vite build
  → frontend/dist/index.html
  → frontend/dist/assets/*
```

静态资源使用带 hash 的文件名并设置长期缓存；`index.html` 使用短缓存或 no-cache。服务端需要为未知前端路由回退到 `index.html`，但 `/api/*` 请求必须交给 API router，不能回退成 HTML。

本地发布采用“前端独立构建 -> 后端独立构建 -> 当前平台 `webui-embed` 编译期内嵌 -> `deploy/` 单个当前平台二进制”的方式。自动 Release workflow 在共享测试通过后，将同一份已测试前端产物分别编译进 Windows x86_64、Linux x86_64 和 macOS ARM64 binary，再统一发布。未知 SPA 路由回退 `index.html`，`/api/*` 始终交给 API router；开发阶段仍可直接使用 `frontend/dist` 做静态检查，但发布运行不依赖该目录或 Node.js。

## 10. 实施顺序和验收

当前阶段 A–D 已实现；`typecheck`、Vitest、生产构建、`webui-embed` 下的静态资源 fallback/HEAD/ETag 测试和 Windows 当前平台三阶段打包已通过。三平台自动 Release workflow 已配置但尚未在 GitHub Actions 实跑；真实浏览器同源 API 的 Network/Storage smoke 与 Linux/macOS 原生发布需要在对应环境中补做，不能以 mock、handler 测试或 Windows 单平台构建替代。

### 阶段 A：工程初始化

- 在 `frontend/` 创建 React + TypeScript + Vite 工程；
- 配置 TypeScript 严格模式、路径别名、环境区分和 `/api` proxy；
- 建立 `app/modules/shared` 目录边界；
- 确认构建物位于 `frontend/dist`，依赖位于 `frontend/node_modules`。

### 阶段 B：鉴权和应用壳

- 完成 login/logout/session API client；
- 实现 Cookie session、路由守卫、过期处理和统一错误展示；
- 完成侧栏、页面布局、加载/空态/错误态和主题基础。

### 阶段 C：只读查询页面

- 按 API 契约实现 Dashboard、runtime、health、statistics、queries、resources 和 system；
- 解析记录主表按“时间、请求、响应、路由、客户端”五列组织，小屏优先保留请求/响应/客户端；响应列并列显示“总耗时”和“主链”，展开区展示精确字段名、完整有界 answer 与 target/actual upstream；cache 行明确标注为缓存生产来源；
- 为统计和健康状态实现有界轮询，不引入实时推送；
- 对每个模块补齐 API mock/fixture 和组件级测试。

### 阶段 D：交付验证

- `vite build` 成功并只产生 `frontend/dist`；
- TypeScript 类型检查和组件测试通过；
- 同源部署时静态路由回退、`/api` 分流和 `401` session 过期流程正确；
- 查询接口的分页、时间范围、错误分类、authenticated 详情白名单与内部字段 deny-list 有后端契约测试；
- 浏览器不会向 DoH 路由发送管理查询，也不会在网络请求或本地存储中出现密码、token 或 SecretRef 实际值。

配置写入和运行时控制不属于上述验收项。后续若启动该能力，应另行增加权限、审计、CSRF、并发 revision/conflict、命令幂等和失败回滚契约，再新增对应前端模块。

## 11. 参考资料

- [React 官方文档](https://react.dev/)
- [Vite 构建和生产部署](https://vite.dev/guide/build)
- [React Router 官方文档](https://reactrouter.com/)
- [TanStack Query React 官方文档](https://tanstack.com/query/latest/docs/framework/react/overview)
- [Ant Design 官方文档](https://ant.design/)
- [FluxDNS 后端架构设计](../backend/architecture.md)
- [FluxDNS 配置参考](../backend/configuration-reference.md)
