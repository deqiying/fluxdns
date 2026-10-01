# 前端页面与查询实现

> 文档状态：有效
>
> 适用范围：已接入路由、页面数据源、查询状态和实际能力范围
>
> 最后核对：2026-09-30（服务状态趋势图两轴单格候选加密到 1.2/1.25 的局部核对；服务状态指标推送与速率卡片口径沿用 2026-09-27，逐秒 RPM 与 CPU 卡片沿用 2026-09-26，解析记录路由列标签行沿用 2026-09-25，其余沿用原核对范围）
>
> 核对基线：`9555c38` 加本次工作树变更；本轮核对范围仅限服务状态趋势图两轴的刻度取值，其余范围按原日期和基线解释

## 路由与数据源

路由由 [`App`](../../../frontend/src/app/App.tsx) 注册，各模块按 Page -> hook -> api -> shared client 访问后端；全部字段以唯一 [v2 OpenAPI](../../../frontend/openapi/management-api-v2.yaml) 为准。

| 路径 | 代码入口 | 数据/功能 |
| --- | --- | --- |
| `/initialize` | [InitializePage](../../../frontend/src/modules/auth/InitializePage.tsx) | setup 状态与首用户创建、竞争冲突刷新 |
| `/login` | [LoginPage](../../../frontend/src/modules/auth/LoginPage.tsx) | 登录签发内存 Bearer，HttpOnly Cookie 仅用于认证刷新 |
| `/dashboard` | [DashboardPage](../../../frontend/src/modules/dashboard/DashboardPage.tsx) | v2 HTTP/WS 的 RSS、CPU 占用、实时 QPS/RPM（卡底附近 600 秒平均值）、在线身份和双单位趋势图 |
| `/queries` | [QueriesPage](../../../frontend/src/modules/queries/QueriesPage.tsx) | v2 cursor 查询、身份/来源/Answer、实时缓冲和稳定详情 |
| `/system-runtime` | [SystemPage](../../../frontend/src/modules/system/SystemPage.tsx) | v2 进程采样、版本和启动时间 |
| `/listeners` | [ListenersPage](../../../frontend/src/modules/listeners/ListenersPage.tsx) | UDP/TCP/DoH 类型化列表、运行状态列与编辑；标题区与 服务状态／解析记录 同构，只显示同步胶囊，不显示 revision |
| `/upstreams` | [UpstreamsPage](../../../frontend/src/modules/upstreams/UpstreamsPage.tsx) | Hosts/DoH/Group 类型化读写及“上游 / 上游组”URL tab；上游组超时用“数字 + 单位”编辑，默认秒 |
| `/dns-settings` | [DnsSettingsPage](../../../frontend/src/modules/dns-settings/DnsSettingsPage.tsx) | DNS/cache/TTL/ECS/详情与 R/G/T 预览保存；存储大小按 KB／MB／GB 可读单位展示 |
| `/strategies` | [StrategiesPage](../../../frontend/src/modules/strategies/StrategiesPage.tsx) | 有序规则和 cache/TTL/ECS 继承/覆盖 |
| `/hosts` | [HostsPage](../../../frontend/src/modules/hosts/HostsPage.tsx) | const/file 来源、运行状态与类型化编辑 |
| `/rule-sets` | [RuleSetsPage](../../../frontend/src/modules/rule-sets/RuleSetsPage.tsx) | const/file/remote 与 json/clash/dat 分支 |
| `/clients` | [ClientsPage](../../../frontend/src/modules/clients/ClientsPage.tsx) | name/client_id 分离、IP/CIDR 和策略覆盖 |
| `/proxies` | [ProxiesPage](../../../frontend/src/modules/proxies/ProxiesPage.tsx) | SOCKS5 SecretRef env/file/内联 URL；内联形式的密码位以占位符 `FLUXDNS_REDACTED_SECRET` 回显，未改动时后端恢复原密码 |
| `/system-settings` | [SystemSettingsPage](../../../frontend/src/modules/system-settings/SystemSettingsPage.tsx) | 启动字段只读、logs enable/level/path 热编辑 |

12 个目标入口都已进入 router；`/dashboard` 与 `/queries` 已接入 v2 HTTP/WS，`/system-runtime` 接入 BC-23 的 v2 进程查询并统一返回版本与启动时间，其余九个 P3 配置入口接入 v2 typed module API。原 `/runtime`、`/health`、`/statistics`、`/resources`、`/system` 不再注册且返回正常 404；旧页面源码、API/hooks、兼容 fixture 和 v1 类型已删除。

所有配置 Duration 字段（DNS 缓存失败 TTL／乐观回答 TTL／最大陈旧时间／快照周期／TTL 覆盖上下限、Hosts 检查周期、规则集更新周期、客户端与策略 TTL 覆盖、上游组主要超时与回退超时）统一用共享 [`DurationInput`](../../../frontend/src/shared/components/DurationInput.tsx) 的「数值 + 单位」编辑，单位只提供毫秒/秒/分钟/小时/天且默认秒；规则集刷新列等摘要展示经 [`formatDurationText`](../../../frontend/src/shared/config/form-values.ts) 输出可感知单位，回填时把后端恒为纳秒的串归一化成紧凑 duration。

## 查询与缓存行为

[`createAppQueryClient`](../../../frontend/src/app/query-client.ts) 默认 staleTime 10 秒、gcTime 5 分钟，重新聚焦/联网可 refetch；mutation 不重试。取消、401、403 不重试，retryable API 错误有限重试并考虑 Retry-After。

dashboard 先取 v2 HTTP 快照再订阅 WS metrics；订阅后的逐秒序列由全量基线和之后每秒的增量帧拼装（[`metricsSeries`](../../../frontend/src/shared/api/metricsSeries.ts)），缺基线、增量与基线不连续或帧内本身有洞时丢弃本地序列并重新订阅，绝不把带洞序列当连续曲线绘图。system runtime 和全局配置状态仍使用 30 秒可见性轮询；页面隐藏时 dashboard 释放订阅，恢复先 refetch。P3 模块页面按 module query key 读取，保存后只失效目标和类型化依赖，不复制整份配置到全局 store。

DashboardPage 的“深色样例/浅色显示”只切换本页 CSS 外观，指标和共享订阅保持不变；离开页面不保存主题。深色文字、缺数提示、双曲线/轴线和按钮采用独立对比色，沿用图表键盘名称与响应式容器。

[`DashboardPage`](../../../frontend/src/modules/dashboard/DashboardPage.tsx) 将 RSS、CPU 占用、实时 QPS、实时 RPM 和在线身份显示为五张独立卡片，窄屏排列成两列；大号数值与单位分开排版，卡底左对齐写口径提示、右对齐写平均值，不可用时保留原始原因说明。两张速率卡片的主值取窗口内原始计数、不做除法：实时 QPS 是最近一个完整秒的请求数（逐秒桶的末位样本），实时 RPM 是过去 60 秒请求数（滚动求和的末位样本），因此与趋势图两条曲线的末端点同口径；平均值统一取近 600 秒窗口，平均 QPS 由后端 `rpm` 标量 ÷ 60 得到、平均 RPM 直接取 `rpm`，后端 `qps` 标量（近 60 秒均值）不再在本页显示。主值与平均值各自独立降级：末位样本不可用或还没有逐秒样本时按暖机/缺口原因显示，不用 0 或旧值替代；单卡宽度放不下时平均值换行并保持右对齐。CPU 与 RSS 复用同一进程采样快照，CPU 以占满一个核心为 100%，多线程可超过 100%。页头“实时连接正常”仅在 WS 为 `open`、快照未过期且查询无错误时出现；过期按本地收到指标的时刻判定，连续 3 个推送周期（3 秒）没有新数据才提示延迟，不与服务端 `sampled_at_ms` 相减，避免两端时钟偏移时状态按推送周期来回切换；延迟、中断和重连分别提示，不用设计稿的正常状态覆盖真实数据。

[`MetricsTrendChart`](../../../frontend/src/modules/dashboard/MetricsTrendChart.tsx) 在同一绘图区显示 QPS 蓝线和 RPM 青绿色线，分别标注左轴请求/秒、右轴请求/分钟；两条线都逐秒一个点，RPM 由 [`rateTrend`](../../../frontend/src/modules/dashboard/rateTrend.ts) 把快照逐秒 `qps_trend`（订阅基线加每秒增量拼装出的序列）与页面本地保留的最近 120 秒缓存合并后按过去 60 秒滚动求和得出，同一秒以最新快照为准，因此不依赖后端分钟级 `rpm_trend`：后端快照自带窗口起点前 60 秒前瞻秒桶，窗口最左一分钟也有完整求和窗口，只有服务启动不足 660 秒时才按 warmup 给出已覆盖秒数；缺口秒会让其后 60 秒断线，相邻可用样本间隔过大也断开连线，都不用部分窗口凑数。两个轴独立线性缩放，`axisMaximum` 把单格刻度取为不小于峰值四分之一的最小易读整数值：倍数取 1/1.2/1.25/1.5/2/2.5/3/4/5/6/8/10，小量级下非整数倍数被过滤成 1/2/3/4/5/6/8/10，10 倍量级起为 120/125/150/200/250/300/400/500/600/800/1000；轴上限恒为单格的四倍且不额外留余量，因此两轴刻度始终是整数并与同一组五条网格线对齐；横轴标签按分钟给出并在窄屏放宽到 2/5 分钟。采样时间、时间范围、横轴和提示框统一为 UTC。窗口外样本不参与刻度或选点，不可用区间不连接，孤立有效样本保留为圆端点；空趋势和全不可用趋势有明确提示。提示框由悬停、点按或键盘聚焦显示，离开交互后收起，避免常驻遮挡窄屏曲线；方向键、Home/End 沿共享时间轴按秒选择各序列最近样本，圆点位于该样本实际时间。ResizeObserver 让 SVG 使用容器像素宽度，保持轴文字大小，并在窄屏减少时间刻度。

[`QueriesPage`](../../../frontend/src/modules/queries/QueriesPage.tsx) 使用 `POST /api/v2/queries/search` 的 opaque previous/next cursor，默认最近 7 天、20 条、发生时间降序；域名、当前匹配客户端、原始 ID/IP、协议、来源、rcode 与结果状态均在服务端分页前过滤，不伪造页码或总数。query key 包含规范化请求，过滤或页大小变化清除 cursor、实时缓冲和详情；旧请求取消，翻页不复用不匹配的数据。

解析记录自动刷新默认开启。初次 HTTP 快照后共享 events client 携带快照 cursor、retention revision 和同一过滤器订阅后端 WebSocket 增量，不轮询整个列表；收到记录按稳定 ID 去重。首页默认倒序且无详情时直接合并，详情打开或浏览历史 cursor 时只进入 500 条/2 MiB 缓冲并显示待更新；超限或服务端 resync 重新取 HTTP 首屏。用户关闭自动刷新或页面不可见时释放订阅，恢复可见先重新同步快照。

非模态 Popover 冻结 record ID 与目录快照。鼠标进入结果单元格 120ms 后打开预览，移到浮窗内仍可查看，离开两者 180ms 后关闭；点击结果固定，点击另一结果切换，浮窗内点击和选择文本不关闭，外部点击关闭。键盘聚焦也可打开，Escape 关闭并恢复原触发按钮焦点。后台新记录、refetch 和旧行的延迟关闭回调不能替换当前详情。

详情显示 canonical qname、Answer 截断计数、strategy/upstream/cache producer 与原始/历史/当前三层客户端事实。qname/Answer 只按文本渲染，缺失耗时不伪造为零；共同保留水位使记录过期时显示明确状态，不按行号寻找替代记录。

解析记录页沿用服务状态的大标题、单句说明和浅色圆角卡片。列表按时间、请求、结果、路由、客户端排列，行高固定为 76px，每个单元格保留两行，超出宽度以省略号显示；窄屏保持表格内部横向滚动。客户端仅显示名称和请求 IP，名称丢失显示未命名/未匹配占位，历史匹配 ID 保留在详情。主筛选为域名、客户端、请求 IP、协议和来源，原始 ID 移到高级筛选。

列表结果第二行仅显示响应耗时与来源标签；详情保留总耗时、主链耗时、响应耗时、发送状态及原始/历史/当前身份。三种耗时来自各自后端测点，不能互相相减或以 HTTP 拉取时间代替；历史未记录的响应耗时显示“未记录”，失败发送不显示成功响应耗时。来源标签保留命中缓存、乐观缓存、缓存过期、请求上游及 Hosts；高级缓存筛选保留相同分类名称，提交值仍为原枚举。

路由第一行从 `listener_name` 经策略和上游目标到实际出口；缓存命中沿用缓存生产出口，详情明确它不是当前后台刷新路由。`RouteChain` 根据实际列宽选择完整链路或“入口 → … → 出口”，首尾名称各自可省略，完整内容仍在 title、无障碍名称及详情中保留。第二行显示 `cache_activity`：后台刷新和实际新建/更新/冲突/失败等结果，不能由 miss/expired 推断写入成功；Hosts 命中不产生写入结果，改为在同一标签行以同级的 `Hosts` 来源标签（`RouteSourceTag`）标注，既不显示无意义的未写入标签，也不留空这一行。后台刷新实际目标和出口在详情中单独展示。

P5 触摸回归发现 Popover 的开闭 key 会替换触发按钮；Escape 关闭后必须在渲染完成时按稳定 record ID 重新取得当前 DOM 节点再恢复焦点，不能缓存即将移除的按钮。新增 TouchA 点击、恶意 Answer 文本、Escape 与焦点恢复联合测试，该轮前端 97 项通过；加入认证缓存回归后的最终套件为 98 项。

[`SystemPage`](../../../frontend/src/modules/system/SystemPage.tsx) 以 `/api/v2/system/runtime` 为进程读数权威：指标区固定为「运行时长／常驻内存／CPU／线程数」四个同构格子（13px 标签、30px 等宽数值、12.5px 口径说明），版本、启动时间、采样时间与采样来源合并为同一卡片内的「运行信息」行，与上方格子共用同一条四列栅格，不再出现 16px 双行日期混进 28px 数值格、或标签列固定 132px 右侧留白的断口。RSS 从十进制 u64 字符串按 BigInt 换算；measurement 不可用时保留后端 reason，不能以零代替。运行时长只从成功响应的 `uptime_seconds` 与前端接收时刻递增，页面隐藏时停止逐秒渲染，重新可见后校正；30 秒轮询或手动刷新会按后端基准重置。同一页再读 `/config/system`（`work_path`）、`/config/state`（活动版本与同步胶囊）、`/config/modules/{listener,dns,hosts,rule_set}`（监听绑定、缓存快照、资源条件）与 `/service/metrics`（在线身份计数），组成「运行环境」与「数据面摘要」两张卡片；进程主机信息随 `/system/runtime` 的 `host` 对象返回，不再单独请求。次级读取失败显示「暂不可用 + 契约错误码」，尚未返回显示占位符，不把加载中渲染成不可用。

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
| 监听入口标题区统一 | [ListenersPage](../../../frontend/src/modules/listeners/ListenersPage.tsx)、`ConfigSyncBadge`、[useConfigState](../../../frontend/src/shared/config/hooks.ts)、[index.css](../../../frontend/src/styles/index.css) | `/listeners` 标题区不再消费 `ConfigStateSummary` 的 revision；胶囊读全局轮询状态，编辑禁用仍按模块读取判定 | 2026-09-24：前端 26 文件 115 项 Vitest 与 `pnpm run typecheck`、生产构建通过；新增用例断言同构短句副标题、胶囊 pending 色调、搜索占位、弹窗标题与 revision 不再渲染 | 未在真实浏览器复验 34px／字距／胶囊尺寸等视觉数值；其他配置页仍保留 revision，未一并调整 |
| Duration 单位统一 | [DurationInput](../../../frontend/src/shared/components/DurationInput.tsx)、[form-values](../../../frontend/src/shared/config/form-values.ts)、`DnsSettingsPage`／`HostsPage`／`RuleSetsPage`／`ClientsPage`／`StrategiesPage`／`UpstreamsPage` | `/upstreams` 上游组超时、`/dns-settings` 六个时长字段（缓存失败 TTL／乐观回答 TTL／最大陈旧时间／快照周期／TTL 上下限）、`/hosts` 与 `/rule-sets` 更新周期、`/clients` 与 `/strategies` TTL 上下限改用「数值 + 单位」，默认秒；规则集刷新列改显示 `1 天` 一类文本 | 2026-09-24：前端 27 文件 120 项 Vitest 与 `pnpm run typecheck`、生产构建通过；用例断言 `5000000000ns`→5 秒、`300000000000ns`→5 分钟、`86400000000000ns`→1 天、键入 `1.5` 不被回显改写单位、未编辑字段归一化后提交 `"1500ms"`／`"3s"`、DNS 弹窗四个必填时长字段单位正确、策略弹窗 `0s` 上下限可保存为 `ttl_override.min = "0s"` | 未在真实浏览器复验控件外观；后端字段上下界（如 `failure_ttl` 1s–5m、`snapshot_interval` 1s–1d）仍只在后端校验，前端只拦必填空值、必填零值与不可表示量级；DNS 保存分支与 `/hosts`、`/clients` 弹窗仅由回填断言覆盖，未逐页断言提交报文 |
| 字节单位与输入控件 | [formatters](../../../frontend/src/shared/formatters/index.ts)、[ByteSizeInput](../../../frontend/src/shared/components/ByteSizeInput.tsx)、[form-values](../../../frontend/src/shared/config/form-values.ts)、`DnsSettingsPage`、`SystemPage`、`DashboardPage` | 字节展示统一走 `formatBytes`（1024 进制自适应 `KB`／`MB`／`GB`／`TB`，删除固定 `MiB` 的 `formatBytesMiB`，进程 RSS 一并切换）；`/dns-settings` 内存上限与 T 参考大小改为「数值 + 单位」编辑，回填先只保留数值 ≥ 1 的单位、再取其中能精确表示（小数不超过导出的 `BYTE_DISPLAY_MAX_FRACTION_DIGITS` = 6）的最大单位，无法换算时 emit `undefined` 交由必填规则报错 | 2026-09-24：前端 28 文件 129 项 Vitest 与 `pnpm run typecheck`、生产构建通过；用例断言 `67_108_864`→`64 MB`、`805_306_368`→`768 MB`、`1_572_864`→`1.5 MB`、`807_337_984`→`769.9375 MB`、`807_306_368`→`788385.125 KB`（MB 单位下需 10 位小数，超上限故退 KB）、1 TiB→`1 TB`、`1_048_577` 仍回落整数 `B`，键入 `1500 GB` 后 `validateFields()` reject 且 emit `undefined`、清空输入同样 reject，以及 `bytesToDisplay`→`bytesFromForm` 无损往返 + 回显数值恒 ≥ 1 的属性扫描 | 后端回显非法字节数时控件显示为空但表单值仍是原值（当前不可达，仅由 OpenAPI 上限与后端校验保证）；未做真实浏览器复验控件外观（本轮已取消） |
| 进程状态卡片重排 | [SystemPage](../../../frontend/src/modules/system/SystemPage.tsx)、[index.css](../../../frontend/src/styles/index.css) | `/system-runtime` 把指标格、采样说明、版本与启动时间合并为同一张卡片，并新增「采样时间」指标格（日期 + 时钟两行）；栅格由四列改五列并同步两列/一列断点；按设计稿移除「程序」项，保留「运行中」状态徽标 | 2026-09-24：前端 28 文件 129 项 Vitest 与 `pnpm run typecheck`、生产构建通过（含系统运行状态用例） | 未做真实浏览器视觉复验（五列布局、卡片边框、窄屏断点与去掉的「程序」项未目视） |
| 系统运行状态重设计 | [SystemPage](../../../frontend/src/modules/system/SystemPage.tsx)、[system/hooks.ts](../../../frontend/src/modules/system/hooks.ts)、[system/api.ts](../../../frontend/src/modules/system/api.ts)、[index.css](../../../frontend/src/styles/index.css)、[ProcessMetrics](../../../frontend/openapi/management-api-v2.yaml) 新增 `HostEnvironment` | `/system-runtime` 改为 4 格指标 + 运行信息行，并新增「运行环境」「数据面摘要」卡片；主机环境随 `/system/runtime` 返回，数据面读 `/config/system`、`/config/state`、`/config/modules/{listener,dns,hosts,rule_set}`、`/service/metrics`；手动刷新重取本页全部查询 | 2026-09-27：前端 31 文件 164 项 Vitest、`pnpm run typecheck`、生产构建与 v2 schema 契约 4 项通过；后端 `cargo fmt --check` 与 `cargo clippy --all-targets --all-features -- -D warnings` 在 Windows msvc 通过 | 未做真实浏览器视觉复验（四列栅格、卡片边框与断点未目视）；Linux/macOS 主机采集分支未在本机编译（本机只有 windows-msvc target）；mock 中 `started_at_ms` 晚于 `sampled_at_ms` 的既有样本未调整 |
| 配置页标题区统一 | [PageFrame](../../../frontend/src/shared/components/PageFrame.tsx)、`ConfigSyncBadge`、[index.css](../../../frontend/src/styles/index.css)、九个配置页 | `/dns-settings`／`/system-settings`／`/strategies`／`/clients`／`/hosts`／`/rule-sets`／`/proxies`／`/system-runtime`／`/upstreams` 标题区统一为同构短句副标题 + 全局同步胶囊，不再显示活动/文件 revision；`/system-runtime` 只保留次要按钮「刷新」并把采样时间移入内容区；三处重复标题区 CSS 合并为共享 `.page-heading` 规则，深色样例经 CSS 变量继承 | 2026-09-24：前端 28 文件 129 项 Vitest 与 `pnpm run typecheck`、生产构建通过；`App.test.tsx` 既有断言（各页标题、监听入口无 revision、系统运行状态读数）保持通过 | 未做真实浏览器视觉复验（34px 字号、字距、胶囊尺寸与窄屏 24px 间距未目视）；深色样例仅按变量继承推导；`QueriesPage`／`DashboardPage` 删除页级规则后的回归未目视 |
| Hosts 命中路由列标签 | [QueriesPage](../../../frontend/src/modules/queries/QueriesPage.tsx) 的路由列 `CellStack` 与 `RouteSourceTag` | `/queries` 路由列第二行在 `cache_activity` 标签之后追加同级 `Hosts` 标签，来源仍取自详情记录的 `source` | 2026-09-25：`pnpm run typecheck` 与 `pnpm run build`（typecheck + vite build）通过；`QueriesPage.test.tsx` 用例断言已同步（打开全部断点后按行断言路由列标签行） | 本轮未运行 Vitest，未在真实浏览器复核标签行高度与窄屏省略；`rule_set`／`synthetic` 来源未加同级标签 |
| 指标延迟判定改用本地到达时间 | [hooks.ts](../../../frontend/src/modules/dashboard/hooks.ts) 的 `isMetricsStale`、[hooks.test.tsx](../../../frontend/src/modules/dashboard/hooks.test.tsx) | 服务状态页头“实时连接正常/指标更新延迟”改按本地收到指标的时刻（`dataUpdatedAt`）判定，不再用服务端 `sampled_at_ms` 与浏览器时钟相减 | 2026-09-27：前端 31 文件 161 项 Vitest（含新增 4 项：3.5 秒时钟偏移下持续推送保持正常、停止推送超阈值判延迟后恢复、无快照与连接错误分支）与 `tsc --noEmit` 通过；回退到旧公式时 2 项新增用例失败 | 未在真实浏览器或真实后端复验页头状态；3 秒阈值仍是前端常量，后端推送周期变化时需同步调整 |
| 速率卡片主值与平均值口径 | [DashboardPage](../../../frontend/src/modules/dashboard/DashboardPage.tsx) 的 `Metric`／`realtimeReadout`／`averageQps`、[rateTrend.ts](../../../frontend/src/modules/dashboard/rateTrend.ts) 的 `latestReadouts`、[index.css](../../../frontend/src/styles/index.css) 的 `.service-status-foot`／`.service-status-average` | `/dashboard` 两张速率卡片主值改为「实时 QPS＝最近一个完整秒的请求数」「实时 RPM＝过去 60 秒请求数」，卡底右对齐显示近 600 秒平均值（平均 QPS＝`rpm` ÷ 60、平均 RPM＝`rpm`）；后端 `qps` 标量不再显示 | 2026-09-27：前端 31 文件 170 项 Vitest（含新增 6 项：末位读数取原始计数、末端不可用不回退、空序列、主值与平均值分别断言、平均值独立降级、无逐秒样本按暖机降级）、`pnpm run typecheck` 与生产构建通过；`App.test.tsx` 断言更新为「13 请求/秒」「平均 QPS 4.25」「314 请求/分钟」「平均 RPM 255」，回退到旧口径时这些断言失败 | 未做真实浏览器视觉复验（本机工作面板浏览器无法访问本地 Vite 端口）：五列卡片底行在 1440 与窄屏的实际换行、深色样例 `--service-strong` 对比度均未目视；仅在等宽设计稿同字体栈下量得底行合计约 124px < 卡内宽 166px |
| 登录与初始化页重设计 | [LoginPage](../../../frontend/src/modules/auth/LoginPage.tsx)、[InitializePage](../../../frontend/src/modules/auth/InitializePage.tsx)、[index.css](../../../frontend/src/styles/index.css) 的 `.login-*` | 登录页由深色 hero + 浅色卡片改为同侧浅色双栏：左栏品牌章鱼图标 + 蓝色 kicker + 40px 标题 + 三条能力说明（实时运行状态／配置变更可追溯／会话自动续期），右栏白底表单列（26px 标题、40px 输入、44px 主按钮、按钮下方续期说明）；≤900px 隐藏左栏、顶部显示品牌行并把表单改为白底圆角卡片；初始化页沿用同一版式（不含能力清单） | 2026-09-27：前端 31 文件 175 项 Vitest 与 `pnpm run typecheck` 通过，`App.test.tsx` 既有登录/初始化断言（两个标题、三个 label、按钮名、过期提示）保持通过；另在真实 Chromium 用 `_fluxdns/` 下临时 file:// 断点预览实测：1440×900 得 879.7px／560.3px 两列、白底表单列 1px 左边框、卡片 372px 居中于 x=974、三条说明间距 61px 无重叠且未越过页脚、无横向溢出；390×844 得单列、左栏 `display:none`、品牌行与面板同为 #f7f7f8、卡片 342px 圆角 12 带 1px #e0e2e6 边框 | 未在真实浏览器目视 antd 控件外观、密码可见性切换与后端联调登录；断点预览页是手工同步的 markup 副本，与 TSX 存在漂移风险；未覆盖 900px 临界值（898／901）与 320px 极窄屏 |
| 趋势图轴刻度改为整数计数 | [MetricsTrendChart](../../../frontend/src/modules/dashboard/MetricsTrendChart.tsx) 的 `AXIS_STEP_MULTIPLIERS`／`AXIS_INTERVALS`／`axisMaximum`／`integerStep`、[MetricsTrendChart.test.tsx](../../../frontend/src/modules/dashboard/MetricsTrendChart.test.tsx) | `/dashboard` 请求趋势图两轴都是请求计数，单格刻度改为「不小于峰值四分之一的易读整数」，轴上限恒为四倍单格并取消原来的 10% 额外余量；单格倍数再补 1.2/1.25 补上 100→150 的 50% 跨度；两轴仍各自独立缩放，共用五条网格线 | 2026-09-30：前端 31 文件 176 项 Vitest（含 1 项用例：RPM 峰值 86 → 100/75/50/25/0、460 → 480/360/240/120/0、500 → 500/375/250/125/0、906 → 1,000/750/500/250/0、暖机无可用样本 → 4/3/2/1/0）、`pnpm run typecheck` 与生产构建通过；从源码读出候选集与分段数后按同一公式离线复算：改动前规则峰值 21/188/86/906 给 40/400/160/1,600（曲线占 53%/47%/54%/57%），首次整数化给 24/200/100/1,000，本批次把 RPM 峰值 460 由 600 收到 480（77% → 96%）、峰值 500 由 600 收到 500（83% → 100%），RPM 区（峰值 40–4000）最差填充由 66.8%（峰值 401）升到 75.0%（峰值 2401） | 峰值 ≤ 4 时整数刻度只能给 0–4 轴，低流量区间仍偏宽：峰值 1 填充 25%、峰值 5 填充 62.5%，这是「整数刻度 + 四等分」的固定下界；未在真实浏览器复验新刻度观感与窄屏 48px 轴宽下的标签宽度 |

P4 完整 Vitest 为 23 文件 99 项，v2 schema contract 4 项、typecheck 与 production build 通过。Windows 使用 `_fluxdns/p4-live/` ConfigV2 和内嵌 debug binary 完成真实登录、Bearer ticket、UDP/SQLite/HTTP/WS、断线 replay、会话失效、稳定详情以及桌面/390×844 验收，浏览器 Console 无 error/warning。P3 配置验收仍见[前端应用](application.md#p3-联合验收2026-09-08)，P4 安全和实时证据见[共享实时连接](application.md#p4-共享实时连接2026-09-09)。

历史 P5 内嵌 release、四视口、服务状态深色样例、触控证据和 98 项前端回归见 [WebUI 联合验收](../webui-acceptance.md)；本轮视觉更新的验证范围见上表，不将历史内嵌验收作为本轮结果。
