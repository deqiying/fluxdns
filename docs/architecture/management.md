# Management 设计

> 文档状态：有效
>
> 适用范围：独立管理面、认证与会话、初始化写入、只读 API 和 SPA 安全边界
>
> 最后评审：2026-09-05（已接受的 v2 契约拆分复核）

## 设计结论

Management 使用独立 HTTP listener 与 Axum router，不扩展 DoH 的有界 DNS parser。框架类型限定在 adapter 内；Runtime、Storage、Resource 和 DNS ports 只暴露领域类型。读数据通过 snapshot 或 `ManagementStorageRead`，不让 handler 持有 SQLx pool。

API 字段、状态码和错误 envelope 的完整权威是 [OpenAPI](../../frontend/openapi/management-api-v1.yaml)，本文不复制 schema。实际 handler、middleware 和参数上限见[管理端实现](../implementation/backend/management.md)。

## 生命周期与失败

- `webui.enable: false` 时不创建管理服务；启用时必须成功绑定，不能静默退化为仅 DNS 服务。
- 正常加载前恢复配置事务；配置严格校验、DNS/management endpoint 冲突检查和依赖准备失败均阻止启动。
- 管理 accept loop 与连接纳入 Supervisor。单连接/请求错误局部失败，不可恢复入口错误或重试耗尽触发进程优雅关闭。
- 停机撤销 session、停止新请求并在统一预算内 drain。management 不能无限延长 DNS/Storage 的关闭时间。
- listener 与浏览器 origin 属于进程配置，变更需重启；用户列表可更新，外部变更撤销既有 session。内部首次写入用指纹识别，不撤销刚签发的 session。

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

- session token 至少具有 256 bit 随机熵，只经 Cookie 传输，服务端有界内存保存元数据，不把 token 暴露给前端存储。
- Cookie 固定 `HttpOnly`、`SameSite=Strict`、`Path=/`，不设置 `Domain`。HTTPS origin 使用 `__Host-fluxdns_session` 与 `Secure`；HTTP origin 使用 `fluxdns_session` 且不能设置 `Secure`。
- session 同时受绝对/空闲期限、全局/单用户容量限制；退出、外部用户 reload 和进程重启使相关 session 失效。具体常量以 [session.rs](../../backend/src/management/session.rs) 为准。
- Management 本身只提供 HTTP；`public_origin` 是浏览器唯一可接受的绝对 HTTP/HTTPS origin，不含凭据、路径、query 或 fragment。
- 同源判断不能根据 `X-Forwarded-Proto` 或 `X-Forwarded-Host` 放宽。Origin/Fetch Metadata、限流、大小/并发/超时保护必须在统一边界实施。
- 前端不能把密码、hash、token 存入 URL、localStorage、sessionStorage 或查询缓存。未经另行评审不增加通用写 API 或自定义 token 方案。

HTTP 直连不提供传输加密，只适用于 loopback/可信隔离管理网；真实浏览器与反向代理观察仍按[验收计划](../plans/webui-v2-management-integration.md)执行。

## 路由、数据与静态文件

路由优先级是 setup/auth、受保护 `/api/v1/*`、未知 `/api/*` 的 JSON 错误、内嵌静态资源、满足条件的 SPA fallback。只有接受 HTML 的无扩展名 GET/HEAD 前端路径可以回退 `index.html`；资源缺失和未知 API 不得伪装成成功页面。

API 使用统一 request ID、错误 envelope 和有界安全错误，不返回 SQL、绝对配置路径、SecretRef、hash、token 或 backtrace。查询使用只读连接、固定模板、参数绑定、分页和时间窗口上限。

所有 authenticated WebUI 用户可以读取 canonical qname、有效 client IP、真实配置 ID、upstream provenance 与有界 answer。该授权范围不等同普通日志/metrics允许这些内容。历史已脱敏记录保留 `legacy_redacted` 和空详情；不得伪造丢失字段。始终禁止 DNS wire、request digest、route 原文与秘密配置进入 API。

静态响应提供 CSP、`nosniff`、正确 MIME、ETag/HEAD/条件缓存；HTML 与 API 使用保守缓存策略，带内容 hash 的资源可长期缓存。生产构建不包含 source map 或 mock worker。是否真的在无外部资源机器上可用由[交付证据](../implementation/delivery.md)证明，不能由 `webui-embed` 名称推断。
