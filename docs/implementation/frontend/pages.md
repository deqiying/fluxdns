# 前端页面与查询实现

> 文档状态：有效
>
> 适用范围：已接入路由、页面数据源、查询状态和实际能力范围
>
> 最后核对：2026-09-08（P1 系统运行状态页面与查询接线核对）
>
> 核对基线：`a47725dbcf57e000213ba1dacb72ef189fc14d7a`

## 路由与数据源

路由由 [`App`](../../../frontend/src/app/App.tsx) 注册，各模块按 Page -> hook -> api -> shared client 访问后端；当前兼容读取与目标管理字段分别以 [v1 OpenAPI](../../../frontend/openapi/management-api-v1.yaml) 和 [v2 OpenAPI](../../../frontend/openapi/management-api-v2.yaml) 为准。

| 路径 | 代码入口 | 数据/功能 |
| --- | --- | --- |
| `/initialize` | [InitializePage](../../../frontend/src/modules/auth/InitializePage.tsx) | setup 状态与首用户创建、竞争冲突刷新 |
| `/login` | [LoginPage](../../../frontend/src/modules/auth/LoginPage.tsx) | 登录签发内存 Bearer，HttpOnly Cookie 仅用于认证刷新 |
| `/dashboard` | [DashboardPage](../../../frontend/src/modules/dashboard/DashboardPage.tsx) | 当前 v1 overview 卡片与不可用原因；菜单名为“服务状态” |
| `/queries` | [QueriesPage](../../../frontend/src/modules/queries/QueriesPage.tsx) | 请求、响应、路由、客户端与展开 answer |
| `/system-runtime` | [SystemPage](../../../frontend/src/modules/system/SystemPage.tsx) | v2 进程采样，以及现有 v1 版本、启动时间和管理能力 |
| `/upstreams` | [PendingUpstreamsPage](../../../frontend/src/app/PendingModulePage.tsx) | “上游 / 上游组”页内 tab 与 URL 状态；无业务数据 |
| `/listeners`、`/dns-settings`、`/strategies`、`/hosts`、`/rule-sets`、`/clients`、`/proxies`、`/system-settings` | [PendingModulePage](../../../frontend/src/app/PendingModulePage.tsx) | 明确暂不可用，不请求或展示演示数据 |

12 个目标入口都已进入 router；`/dashboard` 与 `/queries` 接入当前 v1 查询，`/system-runtime` 接入 BC-23 的 v2 进程查询并复用当前 v1 system 基础信息。`/upstreams` 只有 tab 壳层，其余八个入口为空态。原 `/runtime`、`/health`、`/statistics`、`/resources`、`/system` 不再注册且返回正常 404；对应源码暂留供后续页面提取有效投影或在 FC-15 删除，不代表仍有正式入口。

## 查询与缓存行为

[`createAppQueryClient`](../../../frontend/src/app/query-client.ts) 默认 staleTime 10 秒、gcTime 5 分钟，重新聚焦/联网可 refetch；mutation 不重试。取消、401、403 不重试，retryable API 错误有限重试并考虑 Retry-After。

当前已接线的 dashboard 和 system runtime 进程 hook 使用 30 秒摘要轮询，页面隐藏时返回 false 且不启用后台轮询。queries 的参数进入 query key，使用 `keepPreviousData` 保留翻页期间的数据；这不代表新条件已经返回结果。未注册旧模块的 hooks 不会由当前 router 启动。

[`QueriesPage`](../../../frontend/src/modules/queries/QueriesPage.tsx) 用局部 state 管理页码、pageSize、排序与过滤，默认第 1 页/20 条、按发生时间降序；不是 URL search params 持久化。详情显示 canonical qname、answer、strategy/upstream 与 client，历史脱敏记录用明确占位文字，缺失耗时不伪造为零。

[`SystemPage`](../../../frontend/src/modules/system/SystemPage.tsx) 以 `/api/v2/system/runtime` 为进程读数权威，显示运行时长、RSS、CPU、线程和采样时间；RSS 从十进制 u64 字符串按 BigInt 换算为 MiB。measurement 不可用时保留后端 reason，不能以零代替。运行时长只从成功响应的 `uptime_seconds` 与前端接收时刻递增，页面隐藏时停止逐秒渲染，重新可见后校正；30 秒采样或手动刷新会按后端基准重置。版本、启动时间和管理能力继续独立读取现有 v1 system，失败时只降级这些信息，不隐藏仍有效的 v2 进程读数。

[`PageState`](../../../frontend/src/shared/components/PageState.tsx)、[`SnapshotMeta`](../../../frontend/src/shared/components/SnapshotMeta.tsx) 与 [formatters](../../../frontend/src/shared/formatters/index.ts) 分别处理错误/加载、快照信息与时间/耗时格式。模块直接使用自己的 API 返回值，不复制整份后端配置到全局 store。

## 证据与限制

| 能力 | 代码实现 | 正式入口接线 | 验证证据 | 已知限制 |
| --- | --- | --- | --- | --- |
| 12 入口导航 | route-contract、AppLayout | 三组菜单与受保护路由 | 23 项路由测试；桌面/390×844 fixture 浏览器沿用 FC-01 证据 | 八个入口为空态，upstreams 仍只有 tab 壳层 |
| 两页 v1 查询 | dashboard/queries Page/hooks/api | AppLayout 下已注册 | fixture 浏览器与既有组件测试 | 未作为新服务状态/记录设计验收 |
| 进程状态 | system Page/hooks/api、formatters | `/system-runtime` 已注册 | FC-14 定向 27 项；完整 Vitest 19 文件 80 项；typecheck/build；Windows 真实浏览器/后端可用样本与刷新 | 基础信息仍复用 v1；窄屏和真实不可用 OS 样本未做浏览器验收 |
| 查询过滤与分页 | QueriesPage、query keys | `useQueries(params)` | 本轮静态；存在 `QueriesPage.test.tsx` | 本轮未测宽/窄屏或组合筛选 |
| 历史空详情 | detail_status 分支、formatter | 查询表格与 answer 展开 | 本轮静态；formatter tests 可定位 | 不重建历史丢失值 |

本轮 typecheck、Vitest 和生产 build 已通过。Windows 使用当前 Vite 页面、`_fluxdns/fc14-ui-live-setup/` 隔离配置和 loopback 后端完成真实登录与 `/system-runtime` 检查：页面显示实际 RSS/CPU/thread、版本、启动/采样时间，手动刷新后采样时间与 uptime 推进，浏览器日志无错误。该检查只覆盖桌面可用样本；不可用分支由 MSW 覆盖，窄屏仍留完整浏览器验收。BC-23 更底层的真实 HTTP 证据见[后端 Management 实现](../backend/management.md#p1-服务与进程指标2026-09-08)。
