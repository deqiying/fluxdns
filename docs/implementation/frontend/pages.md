# 前端页面与查询实现

> 文档状态：有效
>
> 适用范围：已接入路由、页面数据源、查询状态和实际能力范围
>
> 最后核对：2026-09-09（P4 实时指标、解析记录与稳定详情）
>
> 核对基线：`309f49bbd22dc725bd54ecf6d8cc213251b63773` 加本次 P4 文档工作树

## 路由与数据源

路由由 [`App`](../../../frontend/src/app/App.tsx) 注册，各模块按 Page -> hook -> api -> shared client 访问后端；全部字段以唯一 [v2 OpenAPI](../../../frontend/openapi/management-api-v2.yaml) 为准。

| 路径 | 代码入口 | 数据/功能 |
| --- | --- | --- |
| `/initialize` | [InitializePage](../../../frontend/src/modules/auth/InitializePage.tsx) | setup 状态与首用户创建、竞争冲突刷新 |
| `/login` | [LoginPage](../../../frontend/src/modules/auth/LoginPage.tsx) | 登录签发内存 Bearer，HttpOnly Cookie 仅用于认证刷新 |
| `/dashboard` | [DashboardPage](../../../frontend/src/modules/dashboard/DashboardPage.tsx) | v2 HTTP/WS 的 RSS、QPS、RPM、在线身份和双单位趋势图 |
| `/queries` | [QueriesPage](../../../frontend/src/modules/queries/QueriesPage.tsx) | v2 cursor 查询、身份/来源/Answer、实时缓冲和稳定详情 |
| `/system-runtime` | [SystemPage](../../../frontend/src/modules/system/SystemPage.tsx) | v2 进程采样、版本和启动时间 |
| `/listeners` | [ListenersPage](../../../frontend/src/modules/listeners/ListenersPage.tsx) | UDP/TCP/DoH 类型化列表、Runtime binding 与编辑 |
| `/upstreams` | [UpstreamsPage](../../../frontend/src/modules/upstreams/UpstreamsPage.tsx) | Hosts/DoH/Group 类型化读写及“上游 / 上游组”URL tab |
| `/dns-settings` | [DnsSettingsPage](../../../frontend/src/modules/dns-settings/DnsSettingsPage.tsx) | DNS/cache/TTL/ECS/详情与 R/G/T 预览保存 |
| `/strategies` | [StrategiesPage](../../../frontend/src/modules/strategies/StrategiesPage.tsx) | 有序规则和 cache/TTL/ECS 继承/覆盖 |
| `/hosts` | [HostsPage](../../../frontend/src/modules/hosts/HostsPage.tsx) | const/file 来源、运行状态与类型化编辑 |
| `/rule-sets` | [RuleSetsPage](../../../frontend/src/modules/rule-sets/RuleSetsPage.tsx) | const/file/remote 与 json/clash/dat 分支 |
| `/clients` | [ClientsPage](../../../frontend/src/modules/clients/ClientsPage.tsx) | name/client_id 分离、IP/CIDR 和策略覆盖 |
| `/proxies` | [ProxiesPage](../../../frontend/src/modules/proxies/ProxiesPage.tsx) | SOCKS5 SecretRef env/file，不回显实际秘密 |
| `/system-settings` | [SystemSettingsPage](../../../frontend/src/modules/system-settings/SystemSettingsPage.tsx) | 启动字段只读、logs enable/level/path 热编辑 |

12 个目标入口都已进入 router；`/dashboard` 与 `/queries` 已接入 v2 HTTP/WS，`/system-runtime` 接入 BC-23 的 v2 进程查询并统一返回版本与启动时间，其余九个 P3 配置入口接入 v2 typed module API。原 `/runtime`、`/health`、`/statistics`、`/resources`、`/system` 不再注册且返回正常 404；旧页面源码、API/hooks、兼容 fixture 和 v1 类型已删除。

## 查询与缓存行为

[`createAppQueryClient`](../../../frontend/src/app/query-client.ts) 默认 staleTime 10 秒、gcTime 5 分钟，重新聚焦/联网可 refetch；mutation 不重试。取消、401、403 不重试，retryable API 错误有限重试并考虑 Retry-After。

dashboard 先取 v2 HTTP 快照再订阅 WS metrics，system runtime 和全局配置状态仍使用 30 秒可见性轮询；页面隐藏时 dashboard 释放订阅，恢复先 refetch。P3 模块页面按 module query key 读取，保存后只失效目标和类型化依赖，不复制整份配置到全局 store。

[`QueriesPage`](../../../frontend/src/modules/queries/QueriesPage.tsx) 使用 `POST /api/v2/queries/search` 的 opaque previous/next cursor，默认最近 7 天、20 条、发生时间降序；域名、当前匹配客户端、原始 ID/IP、协议、来源、rcode 与结果状态均在服务端分页前过滤，不伪造页码或总数。query key 包含规范化请求，过滤或页大小变化清除 cursor、实时缓冲和详情；旧请求取消，翻页不复用不匹配的数据。

解析记录自动刷新默认关闭。开启后共享 events client 携带快照 cursor、retention revision 和同一过滤器订阅；收到记录按稳定 ID 去重。首页默认倒序且无详情时可直接合并，详情打开或浏览历史 cursor 时只进入 500 条/2 MiB 缓冲并显示待更新；超限或服务端 resync 改为未知数量并重新取 HTTP 首屏。非模态 Popover 以稳定 record ID 和目录快照为键，支持 hover、click/touch、focus 与 Escape；新记录、refetch 和延迟 hover 回调都不能把它换成另一行。

详情显示 canonical qname、Answer 截断计数、strategy/upstream/cache producer 与原始/历史/当前三层客户端事实。qname/Answer 只按文本渲染，缺失耗时不伪造为零；共同保留水位使记录过期时显示明确状态，不按行号寻找替代记录。

[`SystemPage`](../../../frontend/src/modules/system/SystemPage.tsx) 以 `/api/v2/system/runtime` 为进程读数权威，显示运行时长、RSS、CPU、线程和采样时间；RSS 从十进制 u64 字符串按 BigInt 换算为 MiB。measurement 不可用时保留后端 reason，不能以零代替。运行时长只从成功响应的 `uptime_seconds` 与前端接收时刻递增，页面隐藏时停止逐秒渲染，重新可见后校正；30 秒采样或手动刷新会按后端基准重置。版本和启动时间同样来自该 v2 响应，进程状态不再依赖旧 system 查询。

[`PageState`](../../../frontend/src/shared/components/PageState.tsx) 与 [formatters](../../../frontend/src/shared/formatters/index.ts) 处理错误/加载和时间/耗时格式；各页面直接展示对应 v2 响应的采样信息，不复制整份后端配置到全局 store。

## 证据与限制

| 能力 | 代码实现 | 正式入口接线 | 验证证据 | 已知限制 |
| --- | --- | --- | --- | --- |
| 12 入口导航 | route-contract、AppLayout | 三组菜单与受保护路由 | 应用测试；1440×900 真实内嵌浏览器逐路由和 390×844 移动 Drawer | FC-15/P5 收口未进入 |
| 实时服务状态 | dashboard Page/hooks/chart | v2 metrics HTTP + WS | Vitest；真实 DNS 流量、浏览器指标变化与可访问图表 | 深色样例和真实 OS failure 未复核 |
| 进程状态 | system Page/hooks/api、formatters | `/system-runtime` 已注册 | FC-14 测试；Windows 真实浏览器/后端可用样本与刷新；P3 窄屏无溢出 | 真实不可用 OS 样本未做浏览器验收 |
| v2 查询与实时记录 | QueriesPage、cursor hooks、realtime buffer | v2 search/detail HTTP + queries WS | Vitest；真实 UDP/SQLite/HTTP/WS、断线 replay/resync | 真实网络慢读饱和和性能未验证 |
| 稳定详情 | record-keyed Popover、detail formatter | 列表结果与按 ID detail | 持续写入下固定 ID、显式查看新记录、桌面/移动浏览器 | 不重建已过期或历史丢失值 |
| P3 配置管理 | 九个 Page、v2 module hooks、ConfigFileStatus | 十模块读写、保留 preview、文件差异/组合采用 | 91 项 P3 Vitest；真实文件/SQLite/UDP/Bearer HTTP/两档浏览器 | P5、Linux/macOS 与性能未验证 |

P4 完整 Vitest 为 23 文件 99 项，v2 schema contract 4 项、typecheck 与 production build 通过。Windows 使用 `_fluxdns/p4-live/` ConfigV2 和内嵌 debug binary 完成真实登录、Bearer ticket、UDP/SQLite/HTTP/WS、断线 replay、会话失效、稳定详情以及桌面/390×844 验收，浏览器 Console 无 error/warning。P3 配置验收仍见[前端应用](application.md#p3-联合验收2026-09-08)，P4 安全和实时证据见[共享实时连接](application.md#p4-共享实时连接2026-09-09)。
