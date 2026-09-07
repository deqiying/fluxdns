# 前端页面与查询实现

> 文档状态：有效
>
> 适用范围：已接入路由、页面数据源、查询状态和实际能力范围
>
> 最后核对：2026-09-08（P1 受保护路由、导航与页面接线核对）
>
> 核对基线：`0c1b8171f335c49c57cfb525bab98605923533bd`

## 路由与数据源

路由由 [`App`](../../../frontend/src/app/App.tsx) 注册，各模块按 Page -> hook -> api -> shared client 访问后端；完整字段留在 [OpenAPI](../../../frontend/openapi/management-api-v1.yaml)。

| 路径 | 代码入口 | 数据/功能 |
| --- | --- | --- |
| `/initialize` | [InitializePage](../../../frontend/src/modules/auth/InitializePage.tsx) | setup 状态与首用户创建、竞争冲突刷新 |
| `/login` | [LoginPage](../../../frontend/src/modules/auth/LoginPage.tsx) | 登录签发内存 Bearer，HttpOnly Cookie 仅用于认证刷新 |
| `/dashboard` | [DashboardPage](../../../frontend/src/modules/dashboard/DashboardPage.tsx) | 当前 v1 overview 卡片与不可用原因；菜单名为“服务状态” |
| `/queries` | [QueriesPage](../../../frontend/src/modules/queries/QueriesPage.tsx) | 请求、响应、路由、客户端与展开 answer |
| `/upstreams` | [PendingUpstreamsPage](../../../frontend/src/app/PendingModulePage.tsx) | “上游 / 上游组”页内 tab 与 URL 状态；无业务数据 |
| `/listeners`、`/dns-settings`、`/strategies`、`/hosts`、`/rule-sets`、`/clients`、`/proxies`、`/system-settings`、`/system-runtime` | [PendingModulePage](../../../frontend/src/app/PendingModulePage.tsx) | 明确暂不可用，不请求或展示演示数据 |

12 个目标入口都已进入 router，但只有 `/dashboard` 与 `/queries` 接入当前 v1 查询；其余入口只是壳层空态。原 `/runtime`、`/health`、`/statistics`、`/resources`、`/system` 不再注册且返回正常 404；对应源码暂留供后续页面提取有效投影或在 FC-15 删除，不代表仍有正式入口。

## 查询与缓存行为

[`createAppQueryClient`](../../../frontend/src/app/query-client.ts) 默认 staleTime 10 秒、gcTime 5 分钟，重新聚焦/联网可 refetch；mutation 不重试。取消、401、403 不重试，retryable API 错误有限重试并考虑 Retry-After。

当前已接线的 dashboard hook 使用 30 秒摘要轮询，页面隐藏时返回 false 且不启用后台轮询。queries 的参数进入 query key，使用 `keepPreviousData` 保留翻页期间的数据；这不代表新条件已经返回结果。未注册旧模块的 hooks 不会由当前 router 启动。

[`QueriesPage`](../../../frontend/src/modules/queries/QueriesPage.tsx) 用局部 state 管理页码、pageSize、排序与过滤，默认第 1 页/20 条、按发生时间降序；不是 URL search params 持久化。详情显示 canonical qname、answer、strategy/upstream 与 client，历史脱敏记录用明确占位文字，缺失耗时不伪造为零。

[`PageState`](../../../frontend/src/shared/components/PageState.tsx)、[`SnapshotMeta`](../../../frontend/src/shared/components/SnapshotMeta.tsx) 与 [formatters](../../../frontend/src/shared/formatters/index.ts) 分别处理错误/加载、快照信息与时间/耗时格式。模块直接使用自己的 API 返回值，不复制整份后端配置到全局 store。

## 证据与限制

| 能力 | 代码实现 | 正式入口接线 | 验证证据 | 已知限制 |
| --- | --- | --- | --- | --- |
| 12 入口导航 | route-contract、AppLayout | 三组菜单与受保护路由 | 22 项路由测试；桌面/390×844 fixture 浏览器 | 10 个入口仅为空态，未接 v2 API |
| 两页 v1 查询 | dashboard/queries Page/hooks/api | AppLayout 下已注册 | fixture 浏览器与既有组件测试 | 未作为新服务状态/记录设计验收 |
| 查询过滤与分页 | QueriesPage、query keys | `useQueries(params)` | 本轮静态；存在 `QueriesPage.test.tsx` | 本轮未测宽/窄屏或组合筛选 |
| 历史空详情 | detail_status 分支、formatter | 查询表格与 answer 展开 | 本轮静态；formatter tests 可定位 | 不重建历史丢失值 |

本轮未执行 typecheck、Vitest、build 或视觉检查；不能用路由存在或 mock fixture 推断浏览器验收完成。
