# 前端页面与查询实现

> 文档状态：有效
>
> 适用范围：已接入路由、页面数据源、查询状态和实际能力范围
>
> 最后核对：2026-09-23（解析记录两行布局、详情交互与默认实时订阅定向核对；其余内容沿用原核对范围）
>
> 核对基线：`2b3b160` 加本次工作树变更；本轮核对解析记录，服务状态及分批历史结果按原日期和基线解释

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

DashboardPage 的“深色样例/浅色显示”只切换本页 CSS 外观，指标和共享订阅保持不变；离开页面不保存主题。深色文字、缺数提示、双曲线/轴线和按钮采用独立对比色，沿用图表键盘名称与响应式容器。

[`DashboardPage`](../../../frontend/src/modules/dashboard/DashboardPage.tsx) 将 RSS、QPS、RPM 和在线身份显示为四张独立卡片，窄屏排列成两列；大号数值与单位分开排版，不可用时保留原始原因说明。页头“实时连接正常”仅在 WS 为 `open`、快照未过期且查询无错误时出现；延迟、中断和重连分别提示，不用设计稿的正常状态覆盖真实数据。

[`MetricsTrendChart`](../../../frontend/src/modules/dashboard/MetricsTrendChart.tsx) 在同一绘图区显示 QPS 蓝线和 RPM 青绿色线，分别标注左轴请求/秒、右轴请求/分钟；两个轴独立线性缩放并采用易读刻度。采样时间、时间范围、横轴和提示框统一为 UTC。窗口外样本不参与刻度或选点，不可用区间不连接，孤立有效样本保留为圆端点；空趋势和全不可用趋势有明确提示。提示框由悬停、点按或键盘聚焦显示，离开交互后收起，避免常驻遮挡窄屏曲线；方向键、Home/End 沿共享时间轴选择各序列最近样本，圆点位于该样本实际时间。ResizeObserver 让 SVG 使用容器像素宽度，保持轴文字大小，并在窄屏减少时间刻度。

[`QueriesPage`](../../../frontend/src/modules/queries/QueriesPage.tsx) 使用 `POST /api/v2/queries/search` 的 opaque previous/next cursor，默认最近 7 天、20 条、发生时间降序；域名、当前匹配客户端、原始 ID/IP、协议、来源、rcode 与结果状态均在服务端分页前过滤，不伪造页码或总数。query key 包含规范化请求，过滤或页大小变化清除 cursor、实时缓冲和详情；旧请求取消，翻页不复用不匹配的数据。

解析记录自动刷新默认开启。初次 HTTP 快照后共享 events client 携带快照 cursor、retention revision 和同一过滤器订阅后端 WebSocket 增量，不轮询整个列表；收到记录按稳定 ID 去重。首页默认倒序且无详情时直接合并，详情打开或浏览历史 cursor 时只进入 500 条/2 MiB 缓冲并显示待更新；超限或服务端 resync 重新取 HTTP 首屏。用户关闭自动刷新或页面不可见时释放订阅，恢复可见先重新同步快照。

非模态 Popover 冻结 record ID 与目录快照。鼠标进入结果单元格 120ms 后打开预览，移到浮窗内仍可查看，离开两者 180ms 后关闭；点击结果固定，点击另一结果切换，浮窗内点击和选择文本不关闭，外部点击关闭。键盘聚焦也可打开，Escape 关闭并恢复原触发按钮焦点。后台新记录、refetch 和旧行的延迟关闭回调不能替换当前详情。

详情显示 canonical qname、Answer 截断计数、strategy/upstream/cache producer 与原始/历史/当前三层客户端事实。qname/Answer 只按文本渲染，缺失耗时不伪造为零；共同保留水位使记录过期时显示明确状态，不按行号寻找替代记录。

解析记录页沿用服务状态的大标题、单句说明和浅色圆角卡片。列表按时间、请求、结果、路由、客户端排列，行高固定为 76px，每个单元格保留两行，超出宽度以省略号显示；窄屏保持表格内部横向滚动。客户端仅显示名称和请求 IP，名称丢失显示未命名/未匹配占位，历史匹配 ID 保留在详情。主筛选为域名、客户端、请求 IP、协议和来源，原始 ID 移到高级筛选。

列表结果第二行仅显示响应耗时与来源标签；详情保留总耗时、主链耗时、响应耗时、发送状态及原始/历史/当前身份。三种耗时来自各自后端测点，不能互相相减或以 HTTP 拉取时间代替；历史未记录的响应耗时显示“未记录”，失败发送不显示成功响应耗时。来源标签保留命中缓存、乐观缓存、缓存过期、请求上游及 Hosts；高级缓存筛选保留相同分类名称，提交值仍为原枚举。

路由第一行从 `listener_name` 经策略和上游目标到实际出口；缓存命中沿用缓存生产出口，详情明确它不是当前后台刷新路由。`RouteChain` 根据实际列宽选择完整链路或“入口 → … → 出口”，首尾名称各自可省略，完整内容仍在 title、无障碍名称及详情中保留。第二行单独显示 `cache_activity`：后台刷新和实际新建/更新/冲突/失败等结果；不能由 miss/expired 推断写入成功，Hosts 不显示无意义的未写入标签。后台刷新实际目标和出口在详情中单独展示。

P5 触摸回归发现 Popover 的开闭 key 会替换触发按钮；Escape 关闭后必须在渲染完成时按稳定 record ID 重新取得当前 DOM 节点再恢复焦点，不能缓存即将移除的按钮。新增 TouchA 点击、恶意 Answer 文本、Escape 与焦点恢复联合测试，该轮前端 97 项通过；加入认证缓存回归后的最终套件为 98 项。

[`SystemPage`](../../../frontend/src/modules/system/SystemPage.tsx) 以 `/api/v2/system/runtime` 为进程读数权威，显示运行时长、RSS、CPU、线程和采样时间；RSS 从十进制 u64 字符串按 BigInt 换算为 MiB。measurement 不可用时保留后端 reason，不能以零代替。运行时长只从成功响应的 `uptime_seconds` 与前端接收时刻递增，页面隐藏时停止逐秒渲染，重新可见后校正；30 秒采样或手动刷新会按后端基准重置。版本和启动时间同样来自该 v2 响应，进程状态不再依赖旧 system 查询。

[`PageState`](../../../frontend/src/shared/components/PageState.tsx) 与 [formatters](../../../frontend/src/shared/formatters/index.ts) 处理错误/加载和时间/耗时格式；各页面直接展示对应 v2 响应的采样信息，不复制整份后端配置到全局 store。

## 证据与限制

| 能力 | 代码实现 | 正式入口接线 | 验证证据 | 已知限制 |
| --- | --- | --- | --- | --- |
| 12 入口导航 | route-contract、AppLayout | 三组菜单与受保护路由 | 应用测试；1440×900 真实内嵌浏览器逐路由和 390×844 移动 Drawer | 旧页面/API 已退出；四档最终视口见联合验收 |
| 实时服务状态 | dashboard Page/hooks/chart | v2 metrics HTTP + WS | Vitest；真实 DNS 流量、浏览器指标变化与可访问图表 | 深色样例和真实 OS failure 未复核 |
| 服务状态视觉与品牌更新 | [DashboardPage](../../../frontend/src/modules/dashboard/DashboardPage.tsx)、[MetricsTrendChart](../../../frontend/src/modules/dashboard/MetricsTrendChart.tsx)、[AppLayout](../../../frontend/src/shared/components/AppLayout.tsx) | 原 `/dashboard` 数据链路及 Vite 图标资源 | 2026-09-22：完整前端 26 文件 110 项测试、typecheck 和生产构建通过；Windows Chromium 生产预览在 1600/1024/768/390/320px 验证布局、键盘/指针选点、浅深色、图标资源及模拟 WS 停止后的过期提示 | 浏览器数据为模拟 HTTP/WS；本轮未重新验证真实 DNS 后端和内嵌 release |
| 进程状态 | system Page/hooks/api、formatters | `/system-runtime` 已注册 | FC-14 测试；Windows 真实浏览器/后端可用样本与刷新；P3 窄屏无溢出 | 真实不可用 OS 样本未做浏览器验收 |
| v2 查询与实时记录 | QueriesPage、cursor hooks、realtime buffer | v2 search/detail HTTP + queries WS | Vitest；真实 UDP/SQLite/HTTP/WS、断线 replay/resync | 真实网络慢读饱和和性能未验证 |
| 解析记录设计与执行事实 | QueriesPage、RequestTrace、详情投影 | 默认 WS、三项耗时、监听入口和缓存操作结果 | 2026-09-22～23：前端 114 项、schema 4 项、生产构建；Chromium 在 1600/1024/768/390/320px 验证固定行高、首尾省略、浮窗与模拟 WS；本机真实 UDP/TCP/DoH、HTTP/WS 及 TTL 过期刷新验证 | 浏览器使用模拟 API；真实后端使用独立 loopback 夹具，未覆盖远程客户端或生产负载 |
| 稳定详情 | record-keyed Popover、detail formatter | 列表结果与按 ID detail | 持续写入下固定 ID、显式查看新记录、桌面/移动浏览器 | 不重建已过期或历史丢失值 |
| P3 配置管理 | 九个 Page、v2 module hooks、ConfigFileStatus | 十模块读写、保留 preview、文件差异/组合采用 | 91 项 P3 Vitest；真实文件/SQLite/UDP/Bearer HTTP/两档浏览器 | Linux/macOS 未验证；Windows 主链路结果见联合验收 |

P4 完整 Vitest 为 23 文件 99 项，v2 schema contract 4 项、typecheck 与 production build 通过。Windows 使用 `_fluxdns/p4-live/` ConfigV2 和内嵌 debug binary 完成真实登录、Bearer ticket、UDP/SQLite/HTTP/WS、断线 replay、会话失效、稳定详情以及桌面/390×844 验收，浏览器 Console 无 error/warning。P3 配置验收仍见[前端应用](application.md#p3-联合验收2026-09-08)，P4 安全和实时证据见[共享实时连接](application.md#p4-共享实时连接2026-09-09)。

历史 P5 内嵌 release、四视口、服务状态深色样例、触控证据和 98 项前端回归见 [WebUI 联合验收](../webui-acceptance.md)；本轮视觉更新的验证范围见上表，不将历史内嵌验收作为本轮结果。
