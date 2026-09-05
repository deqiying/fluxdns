# 前端页面与查询实现

> 文档状态：有效
>
> 适用范围：已接入路由、页面数据源、查询状态和实际能力范围
>
> 最后核对：2026-09-05（路由、hooks 和页面主路径静态核对）
>
> 核对基线：`0f18d5b2ddf67625121fd7e0662e21723362565f`

## 路由与数据源

路由由 [`App`](../../../frontend/src/app/App.tsx) 注册，各模块按 Page -> hook -> api -> shared client 访问后端；完整字段留在 [OpenAPI](../../../frontend/openapi/management-api-v1.yaml)。

| 路径 | 代码入口 | 数据/功能 |
| --- | --- | --- |
| `/initialize` | [InitializePage](../../../frontend/src/modules/auth/InitializePage.tsx) | setup 状态与首用户创建、竞争冲突刷新 |
| `/login` | [LoginPage](../../../frontend/src/modules/auth/LoginPage.tsx) | Cookie session 登录 |
| `/dashboard` | [DashboardPage](../../../frontend/src/modules/dashboard/DashboardPage.tsx) | overview 卡片与不可用原因 |
| `/runtime` | [RuntimePage](../../../frontend/src/modules/runtime/RuntimePage.tsx) | runtime revision、listener 与生效摘要 |
| `/health` | [HealthPage](../../../frontend/src/modules/health/HealthPage.tsx) | 组件健康、异常与恢复状态 |
| `/statistics` | [StatisticsPage](../../../frontend/src/modules/statistics/StatisticsPage.tsx) | 时间/维度/分页聚合统计 |
| `/queries` | [QueriesPage](../../../frontend/src/modules/queries/QueriesPage.tsx) | 请求、响应、路由、客户端与展开 answer |
| `/resources` | [ResourcesPage](../../../frontend/src/modules/resources/ResourcesPage.tsx) | 资源版本、状态与刷新元数据 |
| `/system` | [SystemPage](../../../frontend/src/modules/system/SystemPage.tsx) | 版本/构建/运行功能信息 |

这些页面都已进入 router，不是仅有组件文件。除认证与一次性 setup 外，页面只读；不存在通用配置编辑、用户列表写入、手动资源刷新、缓存清除或直接 DNS query 控件。

## 查询与缓存行为

[`createAppQueryClient`](../../../frontend/src/app/query-client.ts) 默认 staleTime 10 秒、gcTime 5 分钟，重新聚焦/联网可 refetch；mutation 不重试。取消、401、403 不重试，retryable API 错误有限重试并考虑 Retry-After。

dashboard/runtime/health hooks 使用 30 秒摘要轮询，页面隐藏时返回 false 且不启用后台轮询。system hook 使用 5 分钟 staleTime；resources 没有定时轮询。statistics/queries 的参数进入 query key，使用 `keepPreviousData` 保留翻页期间的数据；这不代表新条件已经返回结果。

[`QueriesPage`](../../../frontend/src/modules/queries/QueriesPage.tsx) 用局部 state 管理页码、pageSize、排序与过滤，默认第 1 页/20 条、按发生时间降序；不是 URL search params 持久化。详情显示 canonical qname、answer、strategy/upstream 与 client，历史脱敏记录用明确占位文字，缺失耗时不伪造为零。

[`PageState`](../../../frontend/src/shared/components/PageState.tsx)、[`SnapshotMeta`](../../../frontend/src/shared/components/SnapshotMeta.tsx) 与 [formatters](../../../frontend/src/shared/formatters/index.ts) 分别处理错误/加载、快照信息与时间/耗时格式。模块直接使用自己的 API 返回值，不复制整份后端配置到全局 store。

## 证据与限制

| 能力 | 代码实现 | 正式入口接线 | 验证证据 | 已知限制 |
| --- | --- | --- | --- | --- |
| 七页只读查询 | module Page/hooks/api | AppLayout 下已注册 | 本轮核对 router 与 hooks | 未运行全页真实浏览器/API smoke |
| 查询过滤与分页 | QueriesPage、query keys | `useQueries(params)` | 本轮静态；存在 `QueriesPage.test.tsx` | 本轮未测宽/窄屏或组合筛选 |
| 历史空详情 | detail_status 分支、formatter | 查询表格与 answer 展开 | 本轮静态；formatter tests 可定位 | 不重建历史丢失值 |

本轮未执行 typecheck、Vitest、build 或视觉检查；不能用路由存在或 mock fixture 推断浏览器验收完成。
