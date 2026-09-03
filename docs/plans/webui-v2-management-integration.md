# FluxDNS v2 前后端整合与 WebUI Management Server 实施方案

> 文档状态：有效
>
> 实现状态：已实施（真实浏览器与双平台发布验收待环境执行）
>
> 适用范围：FluxDNS v2 management server、Management API、WebUI 静态资源打包、认证会话、首次用户初始化与配置持久化
>
> 最后核对：2026-09-04
>
> 关联文档：[后端总体架构](../backend/architecture.md) · [配置参考](../backend/configuration-reference.md) · [前端总体架构](../frontend/architecture.md) · [前端开发计划](../frontend/development-plan.md) · [Management API OpenAPI](../../frontend/openapi/management-api-v1.yaml)

## 1. 摘要

FluxDNS v2 应在不改变 DNS 数据面的前提下，引入独立的 management server，并把前端产物与后端编译到同一个 Rust 可执行文件中。开发构建物继续分别保留在 `frontend/dist` 与 `backend/target`；发布脚本只将带内嵌 WebUI 的两个平台二进制复制到 `deploy/`。management server 独占 `webui.address:webui.port`，负责以下能力：

1. 路由并实现 `/api/v1/*` Management API；
2. 托管编译后的 React SPA，且对非 API 的前端路由回退 `index.html`；
3. 提供登录、会话、退出和同源请求保护；
4. 当 `webui.enable: true` 且 `webui.users` 为空时进入 `setup_required` 状态，只开放首次用户初始化能力；
5. 接收一次性明文密码，在内存中生成 Argon2id hash，仅将用户名和 PHC 格式的 `password_hash` 写回配置文件；
6. 通过既有 Runtime、Observability、Resource 和 Storage 边界提供只读管理数据，不让 HTTP handler 直接依赖 DNS 传输实现或 SQLite 细节。

本方案建议使用 management 专用的 `axum`/`hyper` adapter，而不是扩展现有 DoH HTTP 解析器。现有解析器只接受 DNS-over-HTTPS 的 GET/POST 与 `application/dns-message`，不具备通用 JSON、Cookie、静态资源、SPA fallback 和管理端安全中间件所需的契约。框架类型必须限制在 `management` adapter 内，不能泄漏到 Runtime、Storage、Resource 或 DNS ports。

以下三项是进入实现前必须冻结的安全与持久化契约：

- 只提供 HTTP 的 management listener 与唯一 `public_origin`，TLS 由 Nginx 等外部反向代理终止；
- 初始化密码策略、Argon2id 参数和会话 TTL；
- 配置源文件与 `work/config.yaml` 派生快照的可恢复写入规则。

## 2. 背景与现状基线

### 2.1 已有能力

| 领域 | 已有实现 | 代码或契约依据 |
| --- | --- | --- |
| WebUI 配置模型 | `webui.enable/address/port/users` 已存在；用户只有 `name` 与 `password_hash` | [`WebUiDto`](../../backend/src/config/model.rs)、[`ResolvedWebUi`](../../backend/src/config/resolve.rs) |
| hash 格式校验 | 配置层接受 bcrypt `$2a$/$2b$/$2y$` 和 Argon2id PHC 字符串 | [`is_supported_password_hash`](../../backend/src/config/validate.rs) |
| 前端 SPA | React、TypeScript、Vite、同源 `/api/v1` client 与登录页已存在 | [`frontend/src`](../../frontend/src)、[`vite.config.ts`](../../frontend/vite.config.ts) |
| API 契约 | 登录、退出、会话、概览、运行时、健康、统计、查询、资源和系统接口已在 OpenAPI 中定义 | [`management-api-v1.yaml`](../../frontend/openapi/management-api-v1.yaml) |
| 运行时数据 | Runtime snapshot、健康注册表、资源快照、统计与查询明细写入能力已存在 | [`runtime`](../../backend/src/runtime)、[`observability.rs`](../../backend/src/observability.rs)、[`storage`](../../backend/src/storage) |
| 生命周期 | DNS listener、transport、resource、storage、telemetry 已由 `DnsService` 与 `Supervisor` 管理 | [`service.rs`](../../backend/src/service.rs) |

### 2.2 实施前缺口（已关闭项与剩余验收）

以下表格保留方案编写时的缺口基线；代码实施已关闭其中的后端、前端和脚本项，剩余环境验收以本文第 13 节及对应权威文档为准。

| 缺口 | 当前表现 | 对 v2 的影响 |
| --- | --- | --- |
| WebUI feature gate | 已移除；`webui.enable: true` 可启动独立 Management Server | 保留为历史基线，不再是当前限制 |
| management listener/router | 已有独立 HTTP listener、router、Supervisor task 和 graceful shutdown | 需在真实运行环境继续做故障矩阵验收 |
| 通用 HTTP 能力 | `transport::doh` 是 DoH 专用的有界 HTTP/1.x codec | 不应把 JSON API 与静态文件混入 DoH adapter |
| API handler | setup、auth 和七个只读 `/api/v1/*` handler 已实现 | 需补真实浏览器同源 smoke 记录 |
| 认证实现 | Argon2id/bcrypt、SessionStore、Cookie、Origin/Fetch Metadata 和限流已实现 | 需在真实浏览器验证 Cookie/CSP 观察面 |
| 首次初始化 | `/initialize`、setup query、一次性初始化和 409 竞争处理已实现 | 需在真实浏览器复核初始化跳转 |
| 配置写入 | source-preserving writer、双文件 journal、指纹 CAS 和启动恢复已实现 | 需在对应平台补完整 crash/替换矩阵 |
| Storage 读取 port | 独立 `ManagementStorageRead` port 与 SQLite read-only adapter 已实现 | 继续保持 handler 不直接依赖 SQLx |
| 发布产物布局 | 前端 `frontend/dist/` 与后端 `backend/target/` 各自保留，`deploy/` 只存放发布二进制 | release 流程需要先构建前端，再将产物编译进两个目标平台的 Rust binary |

### 2.3 现有约束

- DNS UDP/TCP/DoH 数据面保持现有端口、adapter 和任务边界；management 不复用 DoH route、HTTP session 或端口。
- Management API 继续使用 `/api/v1`，默认只读；初始化用户是本阶段唯一新增的配置写操作。
- 前端不直接读取配置文件、SQLite、日志文件、SecretRef 或 Runtime 内部对象。
- `webui.enable: false` 时不绑定 management socket、不创建 router、SessionStore 或 management 后台任务。
- `webui.enable: true` 且 `users` 被省略或显式为 `[]` 都是合法的初始化状态，不再是启动错误；v2 模型需要为 `users` 增加空列表默认值，并拒绝 `null`。
- 前端独立构建物固定保留在 `frontend/dist/`，后端 Cargo 构建物固定保留在 `backend/target/`；不得通过 `CARGO_TARGET_DIR` 或复制操作让两者改用 `deploy/`。
- `script/package-embedded.ps1` 负责构建并复制 `Linux x86_64` 与 `Windows x86_64` 的内嵌资源二进制到 `deploy/`；`script/dev.ps1 start` 启动发布二进制时必须显式接收 `-ConfigPath`，不在脚本中预设配置文件路径，并由同一脚本提供 `status`/`stop` 生命周期管理。
- 本方案初始内容是实施拆分；当前代码状态以文档头部和第 12–13 节为准，历史基线表不应被解读为当前限制。

## 3. 目标与非目标

### 3.1 目标

1. 发布物中的单个 FluxDNS 可执行文件同时包含后端代码与 WebUI 静态资源，不依赖部署机器上的 Node.js 或外部 `frontend/dist/`。
2. management server 与 DNS 数据面独立绑定、独立路由、统一受 `Supervisor` 管理，并支持有界优雅关闭。
3. 实现当前 OpenAPI 的全部 `/api/v1/*` 路径，并为首次初始化扩展版本兼容的 API 契约。
4. 首次初始化只允许在没有任何 WebUI 用户时执行一次；并发请求不能创建多个“首个用户”。
5. 明文密码只存在于受限请求体和 hash 计算过程，不进入配置、日志、错误、metrics、Debug 输出或临时文件。
6. 配置更新不会覆盖并发的外部编辑；进程崩溃后能判定并恢复配置源文件与派生快照的一致状态。
7. 现有前端从 mock/契约阶段切换为真实同源 API，并覆盖初始化、登录、会话恢复和过期流程。

### 3.2 非目标

- 本阶段不实现多用户管理、改密、找回密码、角色与细粒度 RBAC。
- 本阶段不提供通用配置编辑 API；除了首次用户初始化，不写入其他配置字段。
- 本阶段不引入 WebSocket、SSE、实时日志流或 DNS 请求内容展示。
- 本阶段不改变 DNS 策略、cache、upstream、resource 或 resolve pipeline。
- 本阶段不把静态资源、密码 hash 或 session 持久化到 SQLite。
- 本阶段不承诺旧浏览器、跨域 SPA 或第三方前端客户端；WebUI 采用同源部署。

## 4. 总体架构

```text
                         FluxDNS single binary
┌─────────────────────────────────────────────────────────────────────┐
│ Application / process lifecycle                                     │
│                                                                     │
│  ┌──────────────────── DNS data plane ────────────────────────────┐  │
│  │ UDP/TCP/DoH listener -> transport adapter -> DNS pipeline      │  │
│  └─────────────────────────────────────────────────────────────────┘  │
│                                                                     │
│  ┌────────────────── Management control plane ────────────────────┐  │
│  │ dedicated listener -> axum router                              │  │
│  │   ├─ /api/v1/auth/* -> AuthService / SessionStore              │  │
│  │   ├─ /api/v1/*      -> ManagementQueryService                  │  │
│  │   └─ /*             -> EmbeddedWebAssets / SPA fallback        │  │
│  └─────────────────────────────────────────────────────────────────┘  │
│            │                 │                  │                    │
│       ConfigStore      read-only ports    Runtime/Health/Resource    │
│            │                 │                  │                    │
│  source config +       StorageReadModel      safe snapshots          │
│  work/config.yaml                                                     │
└─────────────────────────────────────────────────────────────────────┘
```

### 4.1 模块建议

| 模块 | 责任 | 禁止事项 |
| --- | --- | --- |
| `management::server` | HTTP 监听、连接生命周期、请求上限、优雅关闭 | 不终止 TLS，不处理 DNS wire，不访问 SQL |
| `management::router` | 路由优先级、中间件、错误和 request ID | 不持有全局可变配置 |
| `management::auth` | setup/login/logout/session、密码验证、限流 | 不序列化配置，不记录凭据 |
| `management::session` | 生成、查找、过期和撤销不透明 session | 不把 session token 持久化到前端存储或配置 |
| `management::assets` | 编译期资源查找、MIME、ETag/cache、SPA fallback | `/api/*` 不能回退 HTML |
| `management::query` | 调用只读 ports，并映射为 API DTO | 不暴露 Runtime/Storage 内部类型 |
| `config::store` | 首次用户的 CAS、YAML 更新、事务和恢复 | 不写 `ResolvedConfig`，不回写 SecretRef 的解析值 |
| `storage::read_model` | 使用独立只读连接执行有界查询 | 不让 handler 拼接 SQL 或持有 writer 连接 |

建议新增以下文件边界，最终命名应遵循实现时的现有模块风格：

```text
backend/src/
├── management/
│   ├── mod.rs
│   ├── server.rs
│   ├── router.rs
│   ├── auth.rs
│   ├── session.rs
│   ├── assets.rs
│   └── query.rs
├── config/
│   └── store.rs
└── storage/
    └── read_model.rs
```

### 4.2 依赖选择

建议将 `axum`、`tower`/必要的 `tower-http` 能力限定在 management adapter，原因如下：

- Management API 需要 JSON、Cookie、method/path 路由、body limit、timeout、request ID 与中间件组合；继续扩展自研 DoH parser 会显著增加协议与安全风险。
- 静态资源不从文件系统托管，可由自定义 `EmbeddedWebAssets` handler 返回，因此不需要 `ServeDir`。
- Runtime、Storage、Resource 与 DNS ports 只暴露领域 DTO/trait，不依赖 `axum` extractor 或 response 类型。

同时新增直接依赖，而不是依赖 `Cargo.lock` 中的传递依赖：

| 能力 | 建议依赖 | 用途 |
| --- | --- | --- |
| 密码 | `argon2`、`password-hash`、`bcrypt` | 新 hash 使用 Argon2id，兼容验证已有 bcrypt |
| session 随机数 | 操作系统 CSPRNG 对应的直接依赖 | 生成至少 256 bit 不透明 token |
| HTTP/router | `axum`、`hyper`、`tower` | management adapter |
| 静态资源 | `rust-embed` 或等价的编译期内嵌库、`mime_guess` | 将 `frontend/dist` 编译进 binary |
| Cookie | 类型安全 Cookie 库 | 解析、签发和清除 session Cookie |

引入前应固定与 Rust `1.98.0` 兼容的版本并审查 feature；不启用未使用的默认 feature。

## 5. Management Server 生命周期

### 5.1 启动顺序

1. `ConfigLoader` 按现有流程读取、解析和 resolve 配置。
2. `ConfigStore` 在正常校验前检查是否存在未完成的进程内配置事务，并按 journal 恢复或拒绝启动。
3. 配置校验移除 v1 的 `webui.enable` feature gate，继续验证地址、端口、用户名唯一性与 hash 格式。
4. 统一预检 DNS bind plan 与 management endpoint 冲突；配置要求启用 WebUI 时，management bind 失败应使启动失败，而不是静默禁用 UI。
5. 初始化 Storage、Runtime 与 management query 依赖。
6. `webui.enable: true` 时创建 `ManagementService`、`AuthState`、`SessionStore` 与独立 listener；`users` 为空也必须启动，但状态为 `setup_required`。
7. 将 management accept loop 和连接任务纳入既有 `Supervisor`；单连接错误只影响当前连接，accept loop 的不可恢复错误标记 management component 失败。

### 5.2 故障与关闭语义

- 启动阶段无法绑定、构造资源索引或恢复配置事务：启动失败。
- 单个非法请求、认证失败、读取查询失败：返回有界错误，不影响 DNS 数据面。
- management accept loop 在重试预算耗尽后：management component 进入 `failed`，触发进程级优雅关闭；避免配置声明已启用但进程长期只提供部分能力。
- 关闭顺序为：停止接收新请求、等待已接收请求的短时 drain、撤销 session、停止 management tasks，再按现有顺序关闭 DNS transport、resource、storage 与 telemetry。总 drain 受既有 shutdown budget 约束。

### 5.3 reload 语义

现有 `process_owned_reload_change` 把整个 `webui` 视为 restart-required。v2 应拆分为：

| 变化 | 行为 |
| --- | --- |
| `webui.enable/address/port/public_origin` | 进程拥有，继续要求重启 |
| `webui.users` | 动态更新 `AuthState`；不重绑 listener，不重建 DNS Runtime |
| management 自己提交的首次用户写入 | 提交成功后原子替换 `AuthState`，并抑制 watcher 对本次指纹的重复 reload |
| 外部编辑 `webui.users` | 严格校验后替换认证快照；已有 session 默认全部撤销 |

配置 watcher 必须识别进程内写入的版本/指纹，不能把首次初始化误报为“WebUI 配置变更需重启”。

## 6. Router 与 HTTP 契约

### 6.1 路由优先级

路由必须按以下边界分流：

1. 精确匹配 `/api/v1/auth/setup`；
2. 匹配其他 `/api/v1/auth/*`；
3. 匹配需要认证的 `/api/v1/*`；
4. 任意未知 `/api/*` 返回 JSON `404`，不得进入静态资源或 SPA fallback；
5. GET/HEAD 的已知静态资源路径返回内嵌资源；
6. 其余没有扩展名、且客户端接受 HTML 的 GET/HEAD 路径回退 `index.html`；
7. 其他方法或资源路径返回普通 `404/405`。

所有 API 响应保持 OpenAPI 的 `ErrorEnvelope`、`X-Request-Id`、JSON `Content-Type` 和状态码语义。错误正文不得包含 SQL、文件绝对路径、hash、session token 或内部 backtrace。

### 6.2 首次初始化 API

在现有 OpenAPI 中新增以下契约：

| 方法与路径 | 认证 | 请求/响应 | 语义 |
| --- | --- | --- | --- |
| `GET /api/v1/auth/setup` | 不需要 | `200 SetupStatus` | 只返回 `required` 或 `ready`，不返回用户名或用户数量 |
| `POST /api/v1/auth/setup` | 不需要，但仅 `required` 时允许 | `SetupRequest -> 201 Session` | 创建首个用户、提交配置、更新认证快照并签发 session |

建议 schema：

```yaml
SetupStatus:
  state: required | ready

SetupRequest:
  username: string
  password: string
```

错误语义：

| 场景 | HTTP | `error.code` |
| --- | --- | --- |
| 用户名或密码不符合策略 | `400` | `VALIDATION_FAILED` |
| 已由另一个请求或外部配置完成初始化 | `409` | `SETUP_ALREADY_COMPLETED` |
| 配置在读取后被外部修改 | `409` | `CONFIG_CONFLICT` |
| hash、配置事务或 session 签发失败 | `500` | `INTERNAL_ERROR` |
| setup/login 超过速率限制 | `429` | `RATE_LIMITED` |

`POST setup` 必须先完成配置持久化，再更新内存认证状态与签发 Cookie。持久化失败时不能只在内存中创建用户，也不能返回成功。

### 6.3 已有 API handler 数据来源

| API | 应用服务数据源 | 约束 |
| --- | --- | --- |
| `/auth/login` | `AuthState` + password verifier + `SessionStore` | 用户不存在与密码错误返回同一错误；未知用户执行 dummy verify 降低计时差异 |
| `/auth/logout` | `SessionStore` | 幂等撤销当前 session，并返回清除 Cookie |
| `/auth/session` | `SessionStore` | 无效、过期或已撤销 session 返回 `401` |
| `/overview` | Runtime、Health、Storage 只读快照的组合 | handler 不自行聚合数据库明细 |
| `/runtime` | `RuntimeSnapshot::summary` 和安全的 bind 摘要 | 不返回 socket、文件句柄、凭据或完整配置 |
| `/health` | `HealthRegistry` 与 management/storage 状态 | 只返回稳定组件名、状态和脱敏 message |
| `/statistics` | `StorageReadModel` | 日期范围、维度和分页严格按 OpenAPI 上限校验 |
| `/queries` | `StorageReadModel` 的安全投影 | 不返回 qname、client IP、DNS wire 或任意 SecretRef |
| `/resources` | `ResourceRegistrySnapshot::summary` | 不返回资源文件正文或远端凭据 |
| `/system` | build metadata、启动时间、uptime、capabilities | 不返回进程环境变量和主机敏感路径 |

为统计与查询新增只读 port。SQLite adapter 使用独立只读连接、绑定参数和固定 SQL 模板；HTTP handler 不允许直接引用 `sqlx` 类型或拼接 SQL。

### 6.4 请求边界

- 只接受 HTTP/1.1，明确设置 header 数量/总长度、URI 长度、JSON body、连接空闲时间、handler timeout 和并发连接上限。
- setup/login 密码字段长度沿用 OpenAPI 上限 `1024`；setup 的最小密码长度建议固定为 `12`，login 不用新策略拒绝已有账号。
- JSON 使用 `deny_unknown_fields` 的请求 DTO；非法 Content-Type、重复关键 header、超限 body 和尾随 JSON 均失败。
- 修改状态的请求校验精确 `Origin == webui.public_origin`，并结合 Fetch Metadata 拒绝 cross-site；缺少 `Origin` 的浏览器请求按固定策略拒绝。
- 登录和 setup 按 peer/可信代理边界与用户名摘要组合限流；metrics 不记录原始用户名。

## 7. 初始化状态机与前端流程

### 7.1 后端状态机

| 当前状态 | 事件 | 结果 |
| --- | --- | --- |
| `disabled` | 任意 WebUI 请求 | 无 listener，连接失败 |
| `setup_required` | `GET setup` | 返回 `required` |
| `setup_required` | 首个合法 `POST setup` | 配置事务提交，切换 `ready`，签发 session |
| `setup_required` | 并发的后续 `POST setup` | CAS 失败，返回 `409` |
| `ready` | `GET setup` | 返回 `ready` |
| `ready` | `POST setup` | 返回 `409`，不能创建第二个用户 |
| `ready` | 合法 login/session | 进入正常认证流程 |

“没有配置 users”定义为源配置省略 `users` 或显式 `users: []`，在 resolve 后统一为 `webui.users.is_empty()`；仅在 `webui.enable: true` 时进入初始化状态。WebUI 禁用时不因为空 users 启动初始化 listener。

### 7.2 前端启动顺序

前端新增 `/initialize` 页面和 setup query，启动顺序调整为：

```text
load SPA
  -> GET /api/v1/auth/setup
     -> required: 只允许 /initialize，其他页面重定向到 /initialize
     -> ready: GET /api/v1/auth/session
        -> authenticated: 进入受保护页面
        -> 401: 进入 /login
```

实施要点：

- `InitializePage` 提供 `username`、`password`、`confirmPassword`；`confirmPassword` 只在浏览器校验，不发送后端。
- 页面不把密码或 token 写入 `localStorage`、`sessionStorage`、URL、query cache、错误对象或 analytics。
- setup 成功直接使用响应中的 `Session` 更新 auth query 并进入 `/dashboard`，避免再次提交密码。
- setup 返回 `409` 时重新获取 setup 状态；若已是 `ready`，清空密码并导航 `/login`。
- `/login` 在 `required` 状态重定向 `/initialize`；`/initialize` 在 `ready` 状态重定向 `/login` 或已认证首页。
- `ProtectedRoute` 在 setup 状态未确定前只展示 loading，不先发受保护 API 请求。
- OpenAPI 更新后重新生成 TypeScript 类型；MSW fixtures 需要分别覆盖 `required` 与 `ready`。

## 8. 密码、Session 与 Web 安全

### 8.1 密码存储

- 新建用户统一使用 Argon2id，存储标准 PHC 字符串；每次使用 CSPRNG 生成独立 salt。
- Argon2id 参数以实现时在支持平台上的基准测试为准，最低目标为 `m >= 19 MiB`、`t >= 2`、`p >= 1`；最终常量与测试写入代码和配置参考，不提供运行时任意调小的配置。
- 为兼容已存在的配置，登录验证继续支持当前校验器接受的 bcrypt 与 Argon2id；首次初始化和后续新 hash 只写 Argon2id。
- 用户名规范化规则必须在配置校验、setup 与 login 共用：去除首尾空白后校验长度和字符规则，但密码不得 trim 或 Unicode 改写。
- `Debug`、tracing、metrics、错误链和 panic context 禁止包含请求 body、明文密码、password hash 与 session token。

### 8.2 Session

- Session ID 使用至少 256 bit CSPRNG 熵，只通过 Cookie 传输；服务端 `SessionStore` 保存 session 元数据和过期时间。
- Cookie 固定 `HttpOnly`、`SameSite=Strict`、`Path=/` 且不设置 `Domain`。`public_origin` 为 HTTPS 时使用 `__Host-fluxdns_session` 并设置 `Secure`；为 HTTP 时使用 `fluxdns_session` 且不能设置 `Secure`。
- Session 采用绝对 TTL 与空闲 TTL，建议初始值分别为 24 小时和 30 分钟；最终值在 v2.0 契约阶段冻结。
- SessionStore 设置全局和单用户上限，定时清理过期项；进程重启后 session 失效。
- 用户列表发生外部 reload、hash 变化或删除时撤销全部 session，避免旧权限继续存在。

### 8.3 HTTP、反向代理与同源前置契约

WebUI Management Server 不实现 TLS 终止，只绑定 HTTP endpoint；浏览器访问的唯一外部 origin 由 `public_origin` 明确给出：

```yaml
webui:
  enable: true
  address: 127.0.0.1
  port: 8080
  public_origin: https://dns.example.com
  users: []
```

- `public_origin` 是单一绝对 `http` 或 `https` origin，不包含凭据、path、query 或 fragment。
- 常规部署由 Nginx 等可信反向代理终止 TLS，FluxDNS 只绑定受保护的本地/内网 HTTP endpoint；`public_origin` 填写浏览器看到的 HTTPS origin。
- 允许浏览器直接通过 HTTP 访问 Management Server；此时 Cookie 不能设置 `Secure`，凭据与 session 也没有传输加密，只适合 loopback 或可信隔离管理网络。
- 同源校验以 `public_origin` 为唯一事实来源，不信任或解析 `X-Forwarded-Proto`、`X-Forwarded-Host` 来动态放宽 Origin。

`public_origin` 缺失或与请求 Origin 不一致时必须拒绝请求，不能根据监听地址或代理 header 静默降级。

## 9. 配置持久化设计

### 9.1 写入对象与权威路径

首次初始化只修改源配置文档中的：

```yaml
webui:
  users:
    - name: <username>
      password_hash: <argon2id PHC string>
```

写入的权威对象是 `ConfigLoadOutput.source_path` 指向的 CLI 源配置，而不是 `ResolvedConfig`。原因是 resolve 后的路径和 SecretRef 已失去原始表达，直接序列化可能把本地绝对路径或秘密解析结果写回磁盘。

如果 `work.path/config.yaml` 与源配置不是同一文件，它是源配置的派生快照。初始化事务必须同步该快照，否则现有 `create_snapshot` 的 no-replace 冲突会导致下一次启动失败。

### 9.2 `ConfigStore` 责任

新增 `ConfigStore`，保存以下上下文：

- canonical `source_path` 与 source fingerprint；
- resolve 后的 `work.path`、snapshot path 与 snapshot fingerprint；
- 进程内排他锁和跨进程 lock file；
- 最近一次内部提交的 config revision/fingerprint；
- 可恢复事务 journal 的路径和状态。

`ConfigStore::create_initial_user` 执行以下流程：

1. 获取进程内锁和 lock file；
2. 重读源配置与快照，比较预期 fingerprint，拒绝覆盖外部并发编辑；
3. 使用与 loader 相同的严格 parser 确认 `webui.users` 仍为空；
4. 计算 Argon2id hash；
5. 在源 YAML 文档模型中只替换或新增 `webui.users`，保留其他字段、SecretRef 文本和路径表达；
6. 将候选内容重新交给当前 `ConfigLoader`/validator，确认除 users 外的 resolved 语义未改变；
7. 在同目录创建权限受限的临时文件，写入、flush、`fsync`；
8. 写入并 `fsync` 事务 journal，记录旧/新 fingerprint 与两个目标；
9. 依次原子替换源配置和派生快照，再 `fsync` 父目录；
10. 删除 journal，发布新的 config revision；
11. 只有提交完成后才更新 `AuthState` 并返回成功。

两个不同目录中的文件无法获得真正的单次原子替换，因此必须使用 journal 提供 crash recovery，不能把两次 rename 描述为“整体原子”。启动时如果发现 journal：

- 目标均为新 fingerprint：清理 journal 后继续；
- 只有一个目标为新 fingerprint：从已校验的 staged candidate 完成另一个目标；
- 目标或 staged 内容不符合 journal：拒绝启动并给出脱敏的恢复错误，不猜测覆盖用户文件。

### 9.3 YAML 编辑策略

配置 DTO 当前只实现 `Deserialize`，不应为了写一个用户而直接给整个 resolved 模型补 `Serialize`。建议引入 source-preserving 的 YAML document adapter，并以以下验收测试约束其行为：

- 只改变 `webui.users` 的语义；其他字段严格 parse 后与提交前一致；
- 保留 SecretRef 原文、相对路径及未修改字段的用户注释；不凭空新增未知配置字段；
- 支持当前示例采用的 block mapping/sequence；不支持的 YAML 表达必须在写入前返回明确错误，不能生成损坏配置；
- 候选文件必须能被当前严格 loader 重新读取；
- 临时文件、journal 与日志不包含明文密码。

实现前应对候选 YAML 编辑库做最小 spike；如果不能稳定保留源文档，则应明确采用“完整规范化重写并可能丢失注释”的产品行为并获得批准，而不是在 text replacement 中猜测缩进和节点范围。

### 9.4 权限与并发

- 新文件沿用源配置权限；创建临时文件时先限制为当前用户可读写，再写入 hash。
- 初始化使用“users 仍为空 + fingerprint 未变化”的双重 CAS；第二个 setup 请求稳定返回 `409`。
- 配置锁等待有固定超时；超时返回服务不可用或冲突，不无限阻塞 request task。
- API、日志和健康信息只报告事务阶段与错误类别，不回显用户名、hash、配置正文或绝对路径。

## 10. 静态资源与单二进制发布

### 10.1 产物目录与命名

构建目录和发布目录严格分离：

| 类型 | 目录/文件 | 说明 |
| --- | --- | --- |
| 前端独立构建物 | `frontend/dist/` | `pnpm run build` 的输出，供前端开发和静态检查使用；不移动到 `deploy/` |
| 后端独立构建物 | `backend/target/<triple>/release/` | Cargo 的原生输出，保留完整 target 层级；不把 `target` 重定向到 `deploy/` |
| Linux 发布二进制 | `deploy/fluxdns-linux-x86_64` | `x86_64-unknown-linux-gnu`，包含编译期内嵌 WebUI |
| Windows 发布二进制 | `deploy/fluxdns-windows-x86_64.exe` | `x86_64-pc-windows-msvc`，包含编译期内嵌 WebUI |

`deploy/` 只保存可分发的最终二进制，已由仓库 `.gitignore` 忽略；不提交其中的个人构建物。脚本只覆盖对应目标的最终文件，并使用同目录临时文件避免留下半成品；不会清理或移动 `frontend/dist/`、`backend/target/` 中的其他内容。

### 10.2 一键打包脚本

`script/package-embedded.ps1` 是仓库唯一的一键发布打包入口，使用 PowerShell 7，必须从仓库根目录或通过脚本绝对路径调用。脚本不安装额外工具，运行前应按[项目环境使用规范](../standards/environment-usage.md)准备 `pnpm`、Rust target 以及对应的跨平台 linker。

release 构建顺序固定为：

1. 先检查后端 manifest 已声明 `webui-embed` feature，并要求运行环境预先准备两个 Rust target/linker；可用 `rustup` 时由脚本核验 target，缺少 feature 时立即失败，缺少 target/linker 则由对应 Cargo 构建步骤失败，避免生成伪发布物；
2. 在 `frontend/` 执行 `pnpm install --frozen-lockfile`；
3. 执行 `pnpm run build`，生成 `frontend/dist/index.html` 与带内容 hash 的 assets；缺少入口文件时立即失败，禁止生成未内嵌资源的伪发布物；
4. 对 `x86_64-unknown-linux-gnu` 和 `x86_64-pc-windows-msvc` 依次执行 `cargo build --locked --release --features webui-embed --target <triple>`；
5. 编译期检查 `index.html` 和 asset manifest 存在，并将整个 `dist` 作为只读字节嵌入 binary；
6. 将 Cargo 输出复制为本节约定的两个 `deploy/` 文件名，保留前端和后端独立构建物。

脚本不得从 `build.rs` 隐式执行 `pnpm install` 或联网下载依赖，也不得自动安装 Rust target/linker。CI/发布脚本显式完成前端构建，再调用 Cargo；这样依赖锁、失败位置和缓存边界可审计。后端日常检查可保留不内嵌 WebUI 的开发 feature，但正式发布 profile 必须启用 `webui-embed`，并在缺少前端产物时失败。

### 10.3 开发服务管理脚本

`script/dev.ps1` 是本地开发和测试时管理 `deploy/` 中单个发布二进制的统一入口，支持 `start`、`status` 和 `stop` 三个动作。`start` 动作的 `-ConfigPath` 是必填参数，脚本先将其解析为普通文件，再以 `run --config <绝对路径>` 传给二进制；脚本不包含任何默认配置文件路径。未显式提供 `-BinaryPath` 时，仅根据当前系统和架构选择对应的 `deploy/` 文件：

```powershell
# Windows x86_64
pwsh -File script/dev.ps1 start -ConfigPath .\_fluxdns\config.yaml

# Linux x86_64（PowerShell 7）
pwsh -File script/dev.ps1 start -ConfigPath ./_fluxdns/config.yaml

# 需要指定其他二进制时，配置文件仍然必须显式传入
pwsh -File script/dev.ps1 start -BinaryPath .\deploy\fluxdns-windows-x86_64.exe -ConfigPath .\_fluxdns\config.yaml

# 查看脚本记录的进程是否仍在运行
pwsh -File script/dev.ps1 status

# 停止脚本记录的进程
pwsh -File script/dev.ps1 stop
```

脚本启动后将 PID、二进制路径、配置路径和启动时间写入 `_fluxdns/dev-process.json`，并将标准输出/标准错误重定向到 `_fluxdns/logs/dev.stdout.log` 与 `_fluxdns/logs/dev.stderr.log`。`status`/`stop` 会同时校验 PID、进程启动时间和可执行文件路径，避免 PID 被复用时误报或误停；检测到失效状态文件时会清理，状态无法安全核验或 JSON 损坏时则拒绝操作并保留文件供人工诊断。`stop` 等待进程退出后删除状态文件；没有状态文件时停止动作幂等返回。脚本不复制、修改或生成配置；配置相对路径的解析仍遵循[本地测试规范](../standards/local-testing.md)和后端 CLI 契约。

### 10.4 静态响应契约

- `index.html`：`Cache-Control: no-cache` 或短缓存；每次可重新验证。
- 文件名含内容 hash 的 assets：`Cache-Control: public, max-age=31536000, immutable`。
- 返回正确 `Content-Type`、`Content-Length`、`ETag`，HEAD 与 GET header 一致且 HEAD 不返回 body。
- 资源 key 由编译期清单产生；拒绝 `..`、反斜杠、重复解码和 NUL，不能映射到宿主文件系统。
- 默认不嵌入 source map；如果调试产物需要 source map，必须作为单独非生产 profile。
- Content Security Policy 至少使用 `default-src 'self'`，按实际 Vite 产物最小放行；不使用内联脚本作为绕过方案。

## 11. 配置契约调整

v2 实施时应同步更新配置模型、示例与权威参考：

| 字段 | v2 语义 |
| --- | --- |
| `webui.enable` | `true` 启动 management server；`false` 不创建任何 WebUI 资源 |
| `webui.address/port` | 独立 management endpoint；与所有 DNS endpoint 统一检测冲突 |
| `webui.public_origin` | 浏览器访问的唯一 origin，用于 Origin 校验和 Cookie 策略 |
| `webui.users` | 可省略或为空；resolve 后均为 `[]` 并表示 `setup_required`，非空表示 `ready` |
| `webui.users[].name` | 严格唯一，沿用统一用户名校验 |
| `webui.users[].password_hash` | 只接受支持的 PHC/bcrypt hash；拒绝明文 `password` 字段 |

配置 validator 不负责实际验证密码，只验证 hash 格式和静态约束。真实 hash 验证只在 `AuthService` 中完成。

## 12. 分阶段实施计划

### 12.1 V2.0：冻结契约与依赖边界

工作项：

- 扩展 OpenAPI 的 setup endpoint/schema、错误码和 HTTP/HTTPS Cookie 差异；
- 冻结仅 HTTP 的 management listener、`public_origin` 配置、Argon2id 参数、session TTL 和密码策略；
- 定义 management query ports 与 API DTO 映射；
- 完成 YAML source-preserving adapter spike；
- 更新 Cargo feature 与 release build 入口设计。

退出条件：OpenAPI、配置 schema、安全边界、持久化失败语义均可测试，不再依赖 handler 内临时决定。

### 12.2 V2.1：Management Server 与静态资源

实现状态：已完成代码与后端 `webui-embed` 定向测试。

工作项：

- 移除 v1 WebUI feature gate，增加 management endpoint 预检；
- 实现 `ManagementService`、独立 listener、router、中间件和 Supervisor 生命周期；
- 建立内嵌前端资源、静态缓存、HEAD、SPA fallback 与 `/api/*` 隔离；
- 增加 Management health component。

退出条件：`webui.enable` 能正确控制 listener，单 binary 在没有外部 `dist` 时可打开 SPA，DoH 行为无变化。

### 12.3 V2.2：认证、Session 与首次初始化

实现状态：已完成代码、配置事务恢复与定向测试。

工作项：

- 实现 Argon2id 生成、Argon2id/bcrypt 验证、dummy verify 与凭据红线；
- 实现 SessionStore、Cookie、Origin/Fetch Metadata 与限流；
- 实现 setup 状态机与 `ConfigStore` 可恢复事务；
- 拆分 process-owned WebUI 字段与动态 users reload。

退出条件：空 users 可一次性初始化；配置中只有 hash；重启后可登录；并发 setup 最多成功一个；崩溃恢复测试通过。

### 12.4 V2.3：只读 Management API

实现状态：已完成。Runtime、Health、Resource 安全投影，Storage 只读 port/SQLite adapter，以及七个受 Session 保护的只读 handler 已接入；定向契约测试覆盖分页边界、日期上限、request ID 和敏感字段 deny-list。

工作项：

- 为 Runtime、Health、Resource 建立安全 snapshot/query adapter；
- 增加 Storage 只读 port 与独立 SQLite read adapter；
- 实现 OpenAPI 中所有只读 handler、分页、filter、request ID 和错误映射；
- 增加敏感字段回归测试。

退出条件：所有现有 `/api/v1/*` 契约测试通过，handler 不直接访问 SQL 或内部可变状态。

### 12.5 V2.4：前端真实集成

实现状态：已完成代码与 MSW contract 测试；真实浏览器同源 smoke 待环境执行。

工作项：

- 生成更新后的 API types；
- 增加 setup query、`/initialize`、路由守卫与并发冲突处理；
- 将 AuthProvider 启动顺序调整为 setup -> session；
- 更新 MSW fixture 与组件测试；真实同源浏览器测试作为第 13.3 节环境验收，不以本地 mock 代替。

退出条件：首次访问、初始化、自动登录、刷新恢复、退出、session 过期和已初始化竞争流程均符合契约。

### 12.6 V2.5：发布与文档收口

实现状态：已完成脚本与文档同步；双平台 target/linker 和发布 binary 验收待环境执行。

工作项：

- 固化 `script/package-embedded.ps1` 的 frontend build -> Rust embed -> 双平台 release binary 步骤，以及 `script/dev.ps1 start -ConfigPath <path>`、`status`、`stop` 生命周期入口；
- 验证 `frontend/dist/`、`backend/target/` 独立保留，`deploy/` 只包含两个约定命名的发布二进制；
- 运行 DNS 与 management 端到端回归；
- 更新根 README、后端/前端架构、配置参考、模块文档、开发计划和示例；
- 将本方案中的稳定事实迁移到权威文档，确认无保留价值后删除本方案与对应索引项。

退出条件：干净环境可复现 Linux x86_64 与 Windows x86_64 两个单 binary；不依赖外部 `frontend/dist/` 或 Node.js 运行；启动脚本没有配置路径默认值；权威文档只描述真实已实现行为，不再保留 v1 WebUI feature gate 结论。

## 13. 验证方案

### 13.1 后端单元与契约测试

- 配置：省略 users、空 users、`users: null`、重复用户名、非法 hash、HTTP/HTTPS public origin、management/DNS bind 冲突。
- 密码：Argon2id 生成与验证、bcrypt 兼容验证、错误密码、未知用户 dummy verify、Debug/错误脱敏。
- setup：合法创建、校验失败、并发 CAS、外部配置冲突、hash 失败、配置提交失败、已初始化重试。
- 配置事务：同路径与双路径写入、每个 crash point 的恢复、权限、journal 损坏、snapshot 冲突、watcher self-change。
- session：随机性、TTL、idle expiry、撤销、容量上限、用户 reload 后失效、Cookie 属性。
- router：body/header/URI 上限、JSON strictness、Origin、Fetch Metadata、rate limit、request ID、超时。
- API：OpenAPI response/status/error conformance、分页边界、过滤器、敏感字段 deny-list。
- 静态资源：MIME、ETag、cache、HEAD、SPA fallback、未知 API、路径穿越、source map 排除。

### 13.2 前端测试

- setup status 为 `required/ready` 的路由矩阵；
- 初始化表单校验、提交、成功自动登录与 `409` 竞争处理；
- setup 未决时不请求受保护数据；
- 登录失败、session 过期、退出和 API `401` 回收；
- 密码和 token 不进入浏览器持久化存储；
- `pnpm run typecheck`、`pnpm run test`、`pnpm run build`。

### 13.3 集成与回归

| 场景 | 期望 |
| --- | --- |
| `webui.enable: false` | management 端口未监听，DNS 正常 |
| `enable: true, users: []` | `/` 打开初始化页；普通 API 被拒绝 |
| 完成 setup | 源配置与 snapshot 只有 username/hash；当前浏览器获得 session |
| 重启 | 状态为 `ready`，可使用同一账号登录 |
| `users` 预配置 | 首次访问进入 login，不允许 setup |
| 删除外部 `frontend/dist` | `deploy/fluxdns-linux-x86_64` 与 `deploy/fluxdns-windows-x86_64.exe` 仍能完整加载 SPA |
| 独立构建物保留 | `frontend/dist/` 与 `backend/target/` 不被移动或重定向，`deploy/` 只产生两个发布二进制 |
| 启动参数 | `dev.ps1 start` 缺少 `-ConfigPath` 时拒绝执行；显式配置路径能传给二进制，`status`/`stop` 不依赖默认配置 |
| 进程生命周期 | `dev.ps1` 记录 PID 和进程身份信息；`status` 能识别运行/失效状态，`stop` 只停止身份匹配的进程 |
| SPA 深链接 | 非 API 路径回退 `index.html`；未知 `/api/*` 返回 JSON 404 |
| management endpoint 冲突 | 启动前失败并给出明确、脱敏错误 |
| DNS 回归 | UDP、TCP、DoH 的既有 contract/smoke test 不受影响 |
| shutdown | management 和 DNS 连接均在预算内 drain/终止，无悬挂任务 |

2026-09-04 已在 Windows release binary 上完成真实 HTTP smoke，并在临时移出 `frontend/dist/` 后确认嵌入 SPA 仍可用；使用显式 `-ConfigPath` 与 `-BinaryPath` 运行 `dev.ps1 start`、`status`、`stop`，已确认进程身份记录、Management API 可访问和停止后状态清理；真实 HTTP 响应已检查 SPA/API 的 CSP、`nosniff`、缓存策略、ETag，并验证 `If-None-Match` 返回 `304` 空 body；in-app browser 已检查 `/initialize` 深链接、表单必填校验和 Console。仍需在支持的平台上补充真实浏览器 Cookie 与 Network/Storage 观察，且仅有 mock 或 handler 单测不能作为该项通过的依据。

## 14. 风险与控制

| 风险 | 影响 | 控制措施 |
| --- | --- | --- |
| 把 DoH parser 扩成通用管理 HTTP | 协议复杂度和安全回归扩散到 DNS 数据面 | 使用独立 management adapter，DoH 只保留 DNS envelope |
| HTTP 直连泄露凭据或 session | 管理权限被窃取 | 文档明确限制在 loopback/可信隔离网络；常规部署使用 Nginx 等反向代理提供 HTTPS |
| 双配置文件无法整体原子 rename | 崩溃后源文件与 snapshot 不一致 | journal、fingerprint、启动恢复与 fail-closed |
| YAML 重写覆盖用户注释/SecretRef | 配置丢失或秘密落盘 | source-preserving adapter、语义 diff、严格回读；不序列化 resolved 模型 |
| 并发 setup 创建多个首用户 | 认证状态不可预测 | 进程锁 + 文件锁 + empty-users/fingerprint CAS |
| handler 直接查询 SQLite | 层次泄漏、阻塞和 SQL 风险 | 独立 `StorageReadModel` port、只读连接、参数绑定和固定上限 |
| 内嵌前端产物陈旧 | binary 与 OpenAPI/前端版本不一致 | 固定脚本顺序、manifest/hash 检查、双平台产物命名和干净环境 E2E |
| management 依赖污染核心层 | 后续维护和测试成本上升 | 框架类型只留在 adapter，应用层通过 trait 与 DTO 交互 |

## 15. 验收标准

本阶段全部完成需同时满足：

1. 一个 release binary 在无外部 WebUI 文件和无 Node.js 的机器上提供 SPA 与 API。
2. management router、listener 和生命周期独立于 DoH，DNS 数据面回归通过。
3. OpenAPI 现有路径与新增 setup 路径均有后端 contract test 和生成的前端类型。
4. `users: []` 首次访问稳定进入初始化页；并发创建最多一个成功。
5. 配置文件与派生 snapshot 只保存用户名和 Argon2id hash，任何可观察输出均无明文密码、hash 或 session token。
6. 配置写入具备并发冲突检测、权限控制、严格回读和 crash recovery 证据。
7. 登录、session 恢复、退出、过期、Origin/Fetch Metadata、限流和 Cookie 属性通过真实浏览器验证。
8. 所有只读 handler 经由稳定 query/snapshot 边界，不直接引用 SQLite 或 DNS transport 内部实现。
9. 根 README、配置参考、后端/前端架构、模块文档、开发计划和示例已与真实实现同步。
10. `script/package-embedded.ps1` 能在准备好的双平台 target/linker 环境中生成 `deploy/fluxdns-linux-x86_64` 和 `deploy/fluxdns-windows-x86_64.exe`；`script/dev.ps1 start` 要求显式 `-ConfigPath`，并可通过 `status`/`stop` 管理已启动进程。

## 16. 实施前评审清单

- [x] 确认 Management Server 只提供 HTTP，不实现 TLS 终止；`public_origin` 接受 HTTP/HTTPS，并据此决定 Cookie `Secure` 策略。
- [x] 确认 session 绝对 TTL、空闲 TTL、Cookie 名称与容量上限。
- [x] 确认 setup 密码最小长度与最终 Argon2id 参数。
- [x] 通过 YAML source-preserving adapter spike；基于 CST 范围只替换 `webui.users`，不支持的表达明确失败。
- [x] 确认 source config 与 snapshot journal 的恢复点和跨平台替换语义。
- [x] 确认 management accept loop 失败触发进程优雅关闭。
- [x] 确认 `/queries` 的安全投影继续禁止 qname、client IP 与 DNS wire。
- [x] 确认 release feature、前端构建入口和缺失 `dist` 时的失败方式。
- [ ] 确认两个 Rust target/linker 的来源和 CI runner；确认 `frontend/dist/`、`backend/target/` 与 `deploy/` 的隔离及两个发布文件名。
- [x] 确认 `dev.ps1 start` 的 `-ConfigPath` 必填行为，未传入时禁止启动且不回退默认路径；`status`/`stop` 保留 PID、启动时间和可执行文件身份校验。
