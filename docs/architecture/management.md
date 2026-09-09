# Management 设计

> 文档状态：有效
>
> 适用范围：独立管理面、认证与会话、HTTP/WS API、初始化写入和 SPA 安全边界
>
> 最后评审：2026-09-09（P4 实时事件、浏览器 WS 鉴权与断线补齐）

## 设计结论

Management 使用独立 HTTP listener 与 Axum router，不扩展 DoH 的有界 DNS parser。框架类型限定在 adapter 内；Runtime、Storage、Resource 和 DNS ports 只暴露领域类型。读数据通过 snapshot 或 `ManagementStorageRead`，不让 handler 持有 SQLx pool。

既有兼容页面 API 字段、状态码和错误 envelope 的权威为 [v1 OpenAPI](../../frontend/openapi/management-api-v1.yaml)，P0 冻结并由 P1-P4 接入的配置、保留、历史、指标和实时事件契约以 [v2 OpenAPI](../../frontend/openapi/management-api-v2.yaml) 为权威；本文不复制 schema。P4 已注册服务指标和解析记录 WS；分阶段接入不承诺长期维护 v1/v2 并行兼容服务，旧路径退出仍归 P5/BC-27，实际边界见[管理端实现](../implementation/backend/management.md#p0-v2-契约)。

v2 配置读写以活动源表达为权威，模块严格白名单，`name` 为管理/引用键，`client_id` 只负责请求身份且普通编辑不可修改。配置先运行时应用后持久化，操作结果和文件同步状态分开；外部变化只提示，不自动 reload。正式链路已把 Bearer/Origin、handler、ConfigMutationOwner、typed client 与 SPA fallback 一并接入；不新增角色管理。

v2 外部差异只投影白名单模块源值；只读字段和认证 hash 仅显示变化类别。沿 D-07 显示源路径及 SecretRef 引用而不读取实际秘密，普通资源 URL query 与管理认证 token 分开。差异按命名空间/name 配对，不猜改名或授予删除能力；完整输出受项数及实际序列化字节预算限制，不通过截断或伪造替代值制造可采用配置。P3 前端把同名项映射为带旧 name 的 update、仅外部项映射为 create，并以一次全局 Candidate 组合采用；未选和受保护变化只能经覆盖确认还原。

服务指标在所有 transport 的同一 Runtime 接纳边界计数，使用进程级有界窗口和脱敏在线身份，不依赖详情存储。进程 RSS、CPU 和线程由一个受监督采样 owner 定期更新，HTTP 与后续 WS 只能读取同一快照，不能按页面或连接重复启动 OS 采样；暖机、容量截断、读取失败、平台不支持和观测中断必须显式不可用，不能填零伪装。

实时事件使用独立的有界 WS owner。浏览器先以业务 Bearer 向同源 ticket 端点换取 30 秒单次凭据，再通过 `Sec-WebSocket-Protocol` 传递；服务端协商响应只保留固定协议名，不能回显 ticket。upgrade 必须精确校验 `public_origin`，不接受 URL query token、业务 Cookie 或 Authorization 后备。握手后持续复核同一 SessionStore，登出、到期、用户认证变化和 shutdown 都回收连接；长连接不受普通 HTTP 15 秒 timeout 误杀，但仍受连接、订阅、帧、队列、心跳和写入预算约束。

解析记录只在详情事务 commit 后按 `stream_epoch + sequence` 发布。HTTP 快照返回 commit cursor 与共同保留 revision，订阅携带二者，服务端只在 epoch、水位和 replay 范围连续时补发；任何进程重启、cursor 缺口、缓冲溢出或保留 revision 变化都返回 `resync_required`。客户端必须按稳定记录 ID 去重并重新读取 HTTP 权威快照，不能把 event time 当游标或宣称无限回放。

## 生命周期与失败

- `webui.enable: false` 时不创建管理服务；启用时必须成功绑定，不能静默退化为仅 DNS 服务。
- 正常加载前恢复配置事务；配置严格校验、DNS/management endpoint 冲突检查和依赖准备失败均阻止启动。
- 管理 accept loop 与连接纳入 Supervisor。单连接/请求错误局部失败，不可恢复入口错误或重试耗尽触发进程优雅关闭。
- 停机撤销 session、停止新请求并在统一预算内 drain。management 不能无限延长 DNS/Storage 的关闭时间。
- listener 与浏览器 origin 属于进程配置，变更需重启；已应用的用户或 password hash 变化撤销既有 session，普通配置和用户排序变化不撤销。内部首次写入用指纹识别，不撤销刚签发的 session；仅观测外部文件不能发布认证或回收会话。

## 初始化与配置事务

```text
users empty -> setup_required
  -> validate credentials -> generate password hash
  -> ConfigStore commit -> publish auth snapshot -> issue session -> ready
```

`GET auth/setup` 只返回 required/ready，不泄漏用户名或数量。setup 只允许空用户状态一次性创建首用户；并发竞争、已有用户或外部文件变更必须显式冲突，不能覆盖配置。

初始化只写源 YAML 的 `webui.users`，不序列化 `ResolvedConfig`，不解析并落盘 SecretRef 值。源文件和派生 snapshot 的两次替换不具备整体原子性，必须通过 staged candidate、fingerprint、journal 与启动恢复保证可恢复性。完整写入不变量唯一维护于 [Config 设计](backend/modules/config.md)，实际 writer 和限制见[管理端实现](../implementation/backend/management.md)。

密码不 trim 或 Unicode 改写。新 hash 使用独立随机 salt 的 Argon2id，兼容验证既有 bcrypt；参数固定于代码并接受平台性能验收，不提供任意调弱的配置。明文、hash、token 不进入 Debug、日志、metrics 或错误正文。

## 会话与同源安全

- 登录后的业务接口只接受 `Authorization: Bearer <access_token>`，不回退到 Cookie 或 URL query。访问 token 与刷新凭据分别使用至少 256 bit 随机熵，服务端复用同一有界会话权威；不引入 JWT、角色或第二份认证数据库。
- 初始化、登录和同源 POST 刷新接口可以返回短期 access token，前端仅在内存保存；普通 session、配置和业务响应不返回认证 token。刷新凭据只经 HttpOnly Cookie 传输，不能当作 Bearer；Cookie 不再直接授权业务请求。
- Cookie 固定 `HttpOnly`、`SameSite=Strict`、`Path=/`，不设置 `Domain`。HTTPS origin 使用 `__Host-fluxdns_session` 与 `Secure`；HTTP origin 使用 `fluxdns_session` 且不能设置 `Secure`。
- session 同时受绝对/空闲期限、全局/单用户容量限制；退出、显式认证内容更新和进程重启使相关 session 失效。具体常量以 [session.rs](../../backend/src/management/session.rs) 为准。
- 并发刷新复用当前访问凭据，临近过期才换发；旧访问凭据只保留到原期限，避免在途请求或其他 tab 被提前注销。退出/会话失效同时撤销关联凭据；前端拒绝迟到刷新结果恢复已结束会话，也不让旧请求的 401 清除新登录。
- Management 在同一独立 listener 提供 HTTP 与 WS；`public_origin` 是浏览器唯一可接受的绝对 HTTP/HTTPS origin，不含凭据、路径、query 或 fragment。WS origin 与 ticket 签发也使用这个权威。
- 同源判断不能根据 `X-Forwarded-Proto` 或 `X-Forwarded-Host` 放宽。Origin/Fetch Metadata、限流、大小/并发/超时保护必须在统一边界实施。
- 前端不能把密码、hash、token 存入 URL、localStorage、sessionStorage 或查询缓存。认证响应使用 no-store，访问 token 在进入 AuthProvider 前剥离；业务写请求的未知结果不触发自动刷新重放。未经另行评审不增加通用写 API。

HTTP 直连不提供传输加密，只适用于 loopback/可信隔离管理网；真实浏览器与反向代理观察的证据边界见[管理端实现](../implementation/backend/management.md)。

## 路由、数据与静态文件

路由优先级是 setup/auth、受保护的 `/api/v1/*` 兼容查询、正式 `/api/v2/*` 配置/历史/指标、WS ticket 与 upgrade、未知 `/api/*` 的 JSON 错误、内嵌静态资源、满足条件的 SPA fallback。只有接受 HTML 的无扩展名 GET/HEAD 前端路径可以回退 `index.html`；资源缺失和未知 API 不得伪装成成功页面。

API 使用统一 request ID、错误 envelope 和有界安全错误；错误正文不返回 SQL、绝对配置路径、SecretRef、hash、token 或 backtrace。仅认证专用成功响应返回 access token，刷新凭据永不进入正文。查询使用只读连接、固定模板、参数绑定、分页和时间窗口上限。

所有 authenticated WebUI 用户可以读取 canonical qname、有效 client IP、真实配置 ID、upstream provenance 与有界 answer。该授权范围不等同普通日志/metrics允许这些内容。历史已脱敏记录保留 `legacy_redacted` 和空详情；不得伪造丢失字段。始终禁止 DNS wire、request digest、route 原文与秘密配置进入 API。

静态响应提供 CSP、`nosniff`、正确 MIME、ETag/HEAD/条件缓存；HTML 与 API 使用保守缓存策略，带内容 hash 的资源可长期缓存。生产构建不包含 source map 或 mock worker。是否真的在无外部资源机器上可用由[交付证据](../implementation/delivery.md)证明，不能由 `webui-embed` 名称推断。
