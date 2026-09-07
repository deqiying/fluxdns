# 前端应用与认证实现

> 文档状态：有效
>
> 适用范围：前端 bootstrap、provider、路由鉴权、HTTP client 与会话回收
>
> 最后核对：2026-09-05（入口、认证状态和 client 静态核对）
>
> 核对基线：`0f18d5b2ddf67625121fd7e0662e21723362565f`

## 入口

[`main.tsx`](../../../frontend/src/main.tsx) 的 `bootstrap` 仅在 DEV 且 `VITE_USE_MOCK_API=true` 时启动 MSW，再渲染 `AppErrorBoundary -> AppProviders -> App`。[`providers.tsx`](../../../frontend/src/app/providers.tsx) 依次组合 Ant Design、QueryClient、BrowserRouter 与 AuthProvider。

[`App.tsx`](../../../frontend/src/app/App.tsx) lazy-load 页面，由 Suspense 展示加载态；`/login` 和 `/initialize` 在 guard 外，其他页面进入 `ProtectedRoute -> AppLayout`。根路径转 `/dashboard`，未知受保护路径展示 NotFound。具体页面见[页面与查询](pages.md)。

## 认证状态

[`AuthProvider`](../../../frontend/src/modules/auth/AuthProvider.tsx) 首先请求 `authKeys.setup`；只有 setup 为 ready 才启用 session query。两者都关闭自动重试，并以 provider 的 loading/error/setupRequired/session 向页面提供状态。

- 初始化：`initializeMutation` 成功后写入 setup ready 和新 session；[`InitializePage`](../../../frontend/src/modules/auth/InitializePage.tsx) 负责表单与冲突后的状态刷新。
- 登录：`performLogin` 将返回 session 写入查询缓存，清除 sessionExpired 标志。
- API `401`：`onUnauthorized` 取消查询、设置 sessionExpired、把 session 置 null，由 guard 统一跳转；这个分支没有调用 `queryClient.clear()`，不能描述为清空全部查询缓存。
- 退出：`performLogout` 的 finally 取消查询、清空 query client、将 session 置 null，然后跳转 login；即使网络退出失败也回收本地状态。

[`ProtectedRoute`](../../../frontend/src/modules/auth/ProtectedRoute.tsx) 按 loading -> error -> setup-required -> no-session -> Outlet 处理。鉴权错误先显示错误页，不直接假定未登录；跳转携带来源 pathname。

## HTTP client 与类型

[`apiRequest`](../../../frontend/src/shared/api/client.ts) 固定 `/api/v1` 前缀、默认 10 秒 timeout，并合并调用者 AbortSignal。P1 业务请求改为 Bearer 且 `credentials: omit`，认证专用请求才携带同源 Cookie，详见下节。它校验 JSON Content-Type、解析错误 envelope，保留 request ID/retry-after；非鉴权请求 `401` 通知统一监听者。普通成功值最终是泛型断言，不是完整 OpenAPI 响应运行时 validator。

接口类型来自 [OpenAPI](../../../frontend/openapi/management-api-v1.yaml) 生成的 [`generated.ts`](../../../frontend/src/shared/api/generated.ts)，[`types.ts`](../../../frontend/src/shared/api/types.ts) 提供前端投影。schema 改动后使用 `generate:api`，命令见[前端 README](../../../frontend/README.md)。

## P1 Bearer 接线（2026-09-08）

[`auth/api.ts`](../../../frontend/src/modules/auth/api.ts) 消费初始化/登录的 `AuthSession`，access token 仅存于共享 client 的模块内存；返回 AuthProvider/查询缓存前重新投影 `user/expires_at`，不透传 token 或额外 session 字段。业务请求和 `GET auth/session` 只附加 Authorization Bearer、明确省略 Cookie。页面重载后，client 先调用同源 `POST auth/refresh` 恢复访问凭据，不读取 HttpOnly Cookie 或浏览器持久存储。

同一认证代次内所有请求共享一次在途刷新，刷新最多 5 秒且各等待方仍受自己的 10 秒/调用者取消约束。一个请求取消不终止其他等待者；登出/401 增加认证代次并禁止迟到刷新恢复会话，新登录不受旧请求迟到 401 影响。刷新只发生在业务请求发送前；已发出的请求返回 401/500 或结果未知均不自动重放。登出仍清空本地状态，失败不等于服务端已撤销，沿用上节的错误边界。

mock 的业务 handler 也要求 Bearer，但其 Cookie/Origin 只由测试状态模拟，不充当生产替代。Vite 继续把 `/api` 透明代理到既有后端，未硬编码令牌或生产 baseURL，正式 client 与 handler 保持同一 v1 前缀，v2 只同步目标 schema。Bearer 测试覆盖并发、取消、迟到结果、写请求不重放、无 token session 投影和登录/登出流程。

真实内嵌 WebUI 的浏览器验证覆盖初始化、页面重载后的 Cookie 刷新/Bearer 业务请求、登出后刷新保持未登录、再次登录及 Cookie 清除。开发者接口只读确认 localStorage/sessionStorage 条目均为 0，`document.cookie` 不可读刷新凭据；Network 只记录请求头是否存在，不输出 token。该验证使用旧壳层的真实后端数据，不证明 FC-01 十二路由、FC-02 公共表单或 v2 配置接口完成。

## 能力与证据

2026-09-07 P0 补充：[`generated-v2.ts`](../../../frontend/src/shared/api/generated-v2.ts) 由 [v2 OpenAPI](../../../frontend/openapi/management-api-v2.yaml) 生成，只有新契约模块消费。现有 `apiRequest`、AuthProvider、Vite 代理、mock 和 App 路由未切换，不提供 v1/v2 选择开关。

[`route-contract.ts`](../../../frontend/src/app/route-contract.ts) 固定 12 个一级路径与配置模块映射，保留 `/dashboard`、`/queries`；上游组仅为 `/upstreams` 页内 tab。它尚未导入 App，FC-01 仍待实施，不能将契约表计为页面完成。

[`shared/config/contract.ts`](../../../frontend/src/shared/config/contract.ts) 直接消费生成类型：草稿固定双 revision，区分预校验/确认/应用/结果未知；客户端普通编辑白名单剔除 `client_id`；操作结果区分同步、仅重试持久化、回读活动值和阻塞；大整数转表单前检查安全范围。这里没有表单组件、网络请求或可变全局 store，FC-02 的真实交互、外部差异工作区与 owner 接线尚未实施。

新增 [4 项 Vitest](../../../frontend/src/shared/config/contract.test.ts) 验证路径数量/映射、编辑身份只读、缺失/显式禁用、操作结果及整数精度；与 Rust 共用的 schema 夹具测试见[交付实现](../delivery.md#前端与接口生成)。这些验证不包含浏览器、深链接刷新或新 API 会话安全。

| 能力 | 代码实现 | 正式入口接线 | 验证证据 | 已知限制 |
| --- | --- | --- | --- | --- |
| setup/session gate | AuthProvider + ProtectedRoute | bootstrap 的 provider/router | P1 认证测试及真实初始化/登录/刷新/登出，见上节 | 十二路由和 v2 切换未验收 |
| 同源请求/取消 | `apiRequest`、unauthorized listener | 各 module API 共用 client | P1 并发刷新/取消/迟到结果测试及真实 Bearer 请求头观察 | 普通泛型响应不是完整运行时 schema 校验 |
| 退出数据清理 | `performLogout` finally | AppLayout 使用 auth logout | 本轮核对实际分支 | 401 与 logout 清理行为不同，不能混写 |
| mock 隔离 | bootstrap DEV gate、Vite 构建 | 显式开发变量启用 | 本轮静态 | mock 不证明后端集成或安全验收 |

2026-09-05 原核对未运行 pnpm 或浏览器；P1 新增的 Bearer 运行证据见上节。历史记录与尚无运行证据的环境边界见[交付证据](../delivery.md)，不把旧壳层的认证回归算作 FC-01/02 完成。
