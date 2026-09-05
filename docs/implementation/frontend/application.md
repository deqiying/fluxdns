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

[`apiRequest`](../../../frontend/src/shared/api/client.ts) 固定 `/api/v1` 前缀、`credentials: same-origin`、默认 10 秒 timeout，并合并调用者 AbortSignal。它校验 JSON Content-Type、解析错误 envelope，保留 request ID/retry-after；非鉴权请求 `401` 通知统一监听者。成功值最终是泛型断言，不是完整 OpenAPI 响应运行时 validator。

接口类型来自 [OpenAPI](../../../frontend/openapi/management-api-v1.yaml) 生成的 [`generated.ts`](../../../frontend/src/shared/api/generated.ts)，[`types.ts`](../../../frontend/src/shared/api/types.ts) 提供前端投影。schema 改动后使用 `generate:api`，命令见[前端 README](../../../frontend/README.md)。

## 能力与证据

| 能力 | 代码实现 | 正式入口接线 | 验证证据 | 已知限制 |
| --- | --- | --- | --- | --- |
| setup/session gate | AuthProvider + ProtectedRoute | bootstrap 的 provider/router | 本轮静态；`App.test.tsx` 有路由/认证测试 | 未运行真实浏览器初始化和 Cookie 观察 |
| 同源请求/取消 | `apiRequest`、unauthorized listener | 各 module API 共用 client | 本轮静态；client/contract tests 可定位 | 泛型不是完整运行时 schema 校验 |
| 退出数据清理 | `performLogout` finally | AppLayout 使用 auth logout | 本轮核对实际分支 | 401 与 logout 清理行为不同，不能混写 |
| mock 隔离 | bootstrap DEV gate、Vite 构建 | 显式开发变量启用 | 本轮静态 | mock 不证明后端集成或安全验收 |

本轮未运行 pnpm、浏览器或 Network/Storage 观察。历史浏览器只覆盖 DOM/Console 的记录见[交付证据](../delivery.md)，剩余真实集成见[v2 计划](../../plans/webui-v2-management-integration.md)。
