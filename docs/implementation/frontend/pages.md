# 前端页面与查询实现

> 文档状态：有效
>
> 适用范围：已接入路由、页面数据源、查询状态和实际能力范围
>
> 最后核对：2026-09-08（P3 十模块配置页面与浏览器验收）
>
> 核对基线：`6eab5599f009aa154c14a0a03b50c21a0c074793` 加本次 P3 文档工作树

## 路由与数据源

路由由 [`App`](../../../frontend/src/app/App.tsx) 注册，各模块按 Page -> hook -> api -> shared client 访问后端；当前兼容读取与目标管理字段分别以 [v1 OpenAPI](../../../frontend/openapi/management-api-v1.yaml) 和 [v2 OpenAPI](../../../frontend/openapi/management-api-v2.yaml) 为准。

| 路径 | 代码入口 | 数据/功能 |
| --- | --- | --- |
| `/initialize` | [InitializePage](../../../frontend/src/modules/auth/InitializePage.tsx) | setup 状态与首用户创建、竞争冲突刷新 |
| `/login` | [LoginPage](../../../frontend/src/modules/auth/LoginPage.tsx) | 登录签发内存 Bearer，HttpOnly Cookie 仅用于认证刷新 |
| `/dashboard` | [DashboardPage](../../../frontend/src/modules/dashboard/DashboardPage.tsx) | 当前 v1 overview 卡片与不可用原因；菜单名为“服务状态” |
| `/queries` | [QueriesPage](../../../frontend/src/modules/queries/QueriesPage.tsx) | 请求、响应、路由、客户端与展开 answer |
| `/system-runtime` | [SystemPage](../../../frontend/src/modules/system/SystemPage.tsx) | v2 进程采样，以及现有 v1 版本、启动时间和管理能力 |
| `/listeners` | [ListenersPage](../../../frontend/src/modules/listeners/ListenersPage.tsx) | UDP/TCP/DoH 类型化列表、Runtime binding 与编辑 |
| `/upstreams` | [UpstreamsPage](../../../frontend/src/modules/upstreams/UpstreamsPage.tsx) | Hosts/DoH/Group 类型化读写及“上游 / 上游组”URL tab |
| `/dns-settings` | [DnsSettingsPage](../../../frontend/src/modules/dns-settings/DnsSettingsPage.tsx) | DNS/cache/TTL/ECS/详情与 R/G/T 预览保存 |
| `/strategies` | [StrategiesPage](../../../frontend/src/modules/strategies/StrategiesPage.tsx) | 有序规则和 cache/TTL/ECS 继承/覆盖 |
| `/hosts` | [HostsPage](../../../frontend/src/modules/hosts/HostsPage.tsx) | const/file 来源、运行状态与类型化编辑 |
| `/rule-sets` | [RuleSetsPage](../../../frontend/src/modules/rule-sets/RuleSetsPage.tsx) | const/file/remote 与 json/clash/dat 分支 |
| `/clients` | [ClientsPage](../../../frontend/src/modules/clients/ClientsPage.tsx) | name/client_id 分离、IP/CIDR 和策略覆盖 |
| `/proxies` | [ProxiesPage](../../../frontend/src/modules/proxies/ProxiesPage.tsx) | SOCKS5 SecretRef env/file，不回显实际秘密 |
| `/system-settings` | [SystemSettingsPage](../../../frontend/src/modules/system-settings/SystemSettingsPage.tsx) | 启动字段只读、logs enable/level/path 热编辑 |

12 个目标入口都已进入 router；`/dashboard` 与 `/queries` 接入当前 v1 查询，`/system-runtime` 接入 BC-23 的 v2 进程查询并复用当前 v1 system 基础信息，其余九个 P3 配置入口接入 v2 typed module API。原 `/runtime`、`/health`、`/statistics`、`/resources`、`/system` 不再注册且返回正常 404；对应源码暂留供 FC-15/P5 按引用收口，不代表仍有正式入口。

## 查询与缓存行为

[`createAppQueryClient`](../../../frontend/src/app/query-client.ts) 默认 staleTime 10 秒、gcTime 5 分钟，重新聚焦/联网可 refetch；mutation 不重试。取消、401、403 不重试，retryable API 错误有限重试并考虑 Retry-After。

dashboard、system runtime 和全局配置状态使用 30 秒可见性轮询，页面隐藏时不启用后台轮询。queries 的参数进入 query key，使用 `keepPreviousData` 保留翻页期间的数据；这不代表新条件已经返回结果。P3 模块页面按 module query key 读取，保存后只失效目标和类型化依赖，不复制整份配置到全局 store。

[`QueriesPage`](../../../frontend/src/modules/queries/QueriesPage.tsx) 用局部 state 管理页码、pageSize、排序与过滤，默认第 1 页/20 条、按发生时间降序；不是 URL search params 持久化。详情显示 canonical qname、answer、strategy/upstream 与 client，历史脱敏记录用明确占位文字，缺失耗时不伪造为零。

[`SystemPage`](../../../frontend/src/modules/system/SystemPage.tsx) 以 `/api/v2/system/runtime` 为进程读数权威，显示运行时长、RSS、CPU、线程和采样时间；RSS 从十进制 u64 字符串按 BigInt 换算为 MiB。measurement 不可用时保留后端 reason，不能以零代替。运行时长只从成功响应的 `uptime_seconds` 与前端接收时刻递增，页面隐藏时停止逐秒渲染，重新可见后校正；30 秒采样或手动刷新会按后端基准重置。版本、启动时间和管理能力继续独立读取现有 v1 system，失败时只降级这些信息，不隐藏仍有效的 v2 进程读数。

[`PageState`](../../../frontend/src/shared/components/PageState.tsx)、[`SnapshotMeta`](../../../frontend/src/shared/components/SnapshotMeta.tsx) 与 [formatters](../../../frontend/src/shared/formatters/index.ts) 分别处理错误/加载、快照信息与时间/耗时格式。模块直接使用自己的 API 返回值，不复制整份后端配置到全局 store。

## 证据与限制

| 能力 | 代码实现 | 正式入口接线 | 验证证据 | 已知限制 |
| --- | --- | --- | --- | --- |
| 12 入口导航 | route-contract、AppLayout | 三组菜单与受保护路由 | 应用测试；1440×900 真实内嵌浏览器逐路由和 390×844 移动 Drawer | P4 实时与 FC-15/P5 收口未进入 |
| 两页 v1 查询 | dashboard/queries Page/hooks/api | AppLayout 下已注册 | fixture 浏览器与既有组件测试 | 未作为新服务状态/记录设计验收 |
| 进程状态 | system Page/hooks/api、formatters | `/system-runtime` 已注册 | FC-14 测试；Windows 真实浏览器/后端可用样本与刷新；P3 窄屏无溢出 | 基础信息仍复用 v1；真实不可用 OS 样本未做浏览器验收 |
| 查询过滤与分页 | QueriesPage、query keys | `useQueries(params)` | 本轮静态；存在 `QueriesPage.test.tsx` | 本轮未测宽/窄屏或组合筛选 |
| 历史空详情 | detail_status 分支、formatter | 查询表格与 answer 展开 | 本轮静态；formatter tests 可定位 | 不重建历史丢失值 |
| P3 配置管理 | 九个 Page、v2 module hooks、ConfigFileStatus | 十模块读写、保留 preview、文件差异/组合采用 | 91 项 Vitest、schema/build；真实文件/SQLite/UDP/Bearer HTTP/两档浏览器 | WS/P4、P5、Linux/macOS 与性能未验证 |

P3 的 typecheck、完整 Vitest、schema contract 与生产 build 通过。Windows 使用 `_fluxdns/p3-live/` ConfigV2 和内嵌 debug binary 完成真实登录、12 路由、日志保存、保留 preview、外部双模块组合采用以及桌面/390×844 验收，浏览器 Console 无 error/warning。真实 HTTP/UDP/SQLite 证据和未验证边界见[后端 Management 实现](../backend/management.md#p1-配置事务与文件操作2026-09-08)及[前端应用](application.md#p3-联合验收2026-09-08)。
