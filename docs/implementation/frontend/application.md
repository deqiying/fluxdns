# 前端应用与认证实现

> 文档状态：有效
>
> 适用范围：前端 bootstrap、provider、路由鉴权、HTTP client 与会话回收
>
> 最后核对：2026-09-08（P3 十模块页面、组合采用与真实浏览器验收）
>
> 核对基线：`6eab5599f009aa154c14a0a03b50c21a0c074793` 加本次 P3 文档工作树

## 入口

[`main.tsx`](../../../frontend/src/main.tsx) 的 `bootstrap` 仅在 DEV 且 `VITE_USE_MOCK_API=true` 时启动 MSW，再渲染 `AppErrorBoundary -> AppProviders -> App`。[`providers.tsx`](../../../frontend/src/app/providers.tsx) 依次组合 Ant Design、QueryClient、BrowserRouter 与 AuthProvider。

[`App.tsx`](../../../frontend/src/app/App.tsx) lazy-load 页面，由 Suspense 展示加载态；`/login` 和 `/initialize` 在 guard 外，其他页面进入 `ProtectedRoute -> AppLayout`。根路径转 `/dashboard`，未知受保护路径展示 NotFound。受保护壳层消费 [`route-contract.ts`](../../../frontend/src/app/route-contract.ts) 注册 12 个一级路径，具体接线见[页面与查询](pages.md)。

## 认证状态

[`AuthProvider`](../../../frontend/src/modules/auth/AuthProvider.tsx) 首先请求 `authKeys.setup`；只有 setup 为 ready 才启用 session query。两者都关闭自动重试，并以 provider 的 loading/error/setupRequired/session 向页面提供状态。

- 初始化：`initializeMutation` 成功后写入 setup ready 和新 session；[`InitializePage`](../../../frontend/src/modules/auth/InitializePage.tsx) 负责表单与冲突后的状态刷新。
- 登录：`performLogin` 将返回 session 写入查询缓存，清除 sessionExpired 标志。
- API `401`：`onUnauthorized` 取消查询、设置 sessionExpired、把 session 置 null，由 guard 统一跳转；这个分支没有调用 `queryClient.clear()`，不能描述为清空全部查询缓存。
- 退出：`performLogout` 的 finally 取消查询、清空 query client、将 session 置 null，然后跳转 login；即使网络退出失败也回收本地状态。

[`ProtectedRoute`](../../../frontend/src/modules/auth/ProtectedRoute.tsx) 按 loading -> error -> setup-required -> no-session -> Outlet 处理。鉴权错误先显示错误页，不直接假定未登录；跳转携带来源 pathname。

## HTTP client 与类型

[`apiRequest`](../../../frontend/src/shared/api/client.ts) 固定 `/api/v1` 前缀，[`apiV2Request`](../../../frontend/src/shared/api/client.ts)固定 `/api/v2` 前缀；二者共用默认 10 秒 timeout、调用者 AbortSignal、内存 Bearer 和错误处理，但 v2 调用不会改变认证刷新仍使用 v1 专用端点。P1 业务请求使用 Bearer 且 `credentials: omit`，认证专用请求才携带同源 Cookie，详见下节。client 校验 JSON Content-Type、解析错误 envelope，保留 request ID/retry-after 及受限字段错误；非鉴权请求 `401` 通知统一监听者。普通成功值最终是泛型断言，不是完整 OpenAPI 响应运行时 validator。

既有接口类型来自 [v1 OpenAPI](../../../frontend/openapi/management-api-v1.yaml) 生成的 [`generated.ts`](../../../frontend/src/shared/api/generated.ts)，P3 配置和保留类型来自 [v2 OpenAPI](../../../frontend/openapi/management-api-v2.yaml) 生成的 [`generated-v2.ts`](../../../frontend/src/shared/api/generated-v2.ts)；[`types.ts`](../../../frontend/src/shared/api/types.ts) 只提供既有前端投影。schema 改动后使用 `generate:api`，命令见[前端 README](../../../frontend/README.md)。

## P1 Bearer 接线（2026-09-08）

[`auth/api.ts`](../../../frontend/src/modules/auth/api.ts) 消费初始化/登录的 `AuthSession`，access token 仅存于共享 client 的模块内存；返回 AuthProvider/查询缓存前重新投影 `user/expires_at`，不透传 token 或额外 session 字段。业务请求和 `GET auth/session` 只附加 Authorization Bearer、明确省略 Cookie。页面重载后，client 先调用同源 `POST auth/refresh` 恢复访问凭据，不读取 HttpOnly Cookie 或浏览器持久存储。

同一认证代次内所有请求共享一次在途刷新，刷新最多 5 秒且各等待方仍受自己的 10 秒/调用者取消约束。一个请求取消不终止其他等待者；登出/401 增加认证代次并禁止迟到刷新恢复会话，新登录不受旧请求迟到 401 影响。刷新只发生在业务请求发送前；已发出的请求返回 401/500 或结果未知均不自动重放。登出仍清空本地状态，失败不等于服务端已撤销，沿用上节的错误边界。

mock 的业务 handler 也要求 Bearer，但其 Cookie/Origin 只由测试状态模拟，不充当生产替代。Vite 继续把 `/api` 透明代理到既有后端，未硬编码令牌或生产 baseURL；现有页面与认证 handler 保持 v1，配置公共 client 明确选择 v2，不提供运行时任意版本开关。Bearer 测试覆盖并发、取消、迟到结果、写请求不重放、无 token session 投影和登录/登出流程。

真实内嵌 WebUI 的浏览器验证覆盖初始化、页面重载后的 Cookie 刷新/Bearer 业务请求、登出后刷新保持未登录、再次登录及 Cookie 清除。开发者接口只读确认 localStorage/sessionStorage 条目均为 0，`document.cookie` 不可读刷新凭据；Network 只记录请求头是否存在，不输出 token。该验证使用旧壳层的真实后端数据，不证明 FC-01 十二路由、FC-02 公共表单或 v2 配置接口完成。

## P1 应用壳层（2026-09-08）

[`AppLayout`](../../../frontend/src/shared/components/AppLayout.tsx) 从同一 `managementRoutes` 契约生成“监控 / DNS 管理 / 系统”三组 12 个一级入口，使用 Lucide 图标、浅色侧栏、面包屑、当前用户与图标化登出/折叠控件。桌面侧栏独立滚动；小于 720px 时改用 Drawer，不缩放固定宽画布。未知路径不选择任一菜单项，旧 `/runtime`、`/health`、`/statistics`、`/resources`、`/system` 路径不兼容跳转。

`/dashboard` 和 `/queries` 继续消费当前 v1 真实只读数据，`/system-runtime` 接入 v2 进程读数；P3 已让其余九个配置入口全部消费 v2 类型化读写，不再挂载 [`PendingModulePage`](../../../frontend/src/app/PendingModulePage.tsx)。`/upstreams` 的“上游 / 上游组”tab 仍以 `tab=groups` 进入浏览器历史。主题 token 使用浅灰导航、白工作区、蓝色主操作及独立成功/警告/错误色；未增加暗色全站主题。

Windows 浏览器 fixture 验证覆盖默认桌面、390×844、移动 Drawer 跳转、上游 tab URL、Console warning/error 为空；Vitest 定向路由测试 22 项通过。fixture 不证明 v2 handler、真实配置页面、深链接静态 fallback 或生产内嵌资源已接线；已接入页面仍使用 `/api/v1`。

## P1 配置交互基础（2026-09-08）

[`shared/config/api.ts`](../../../frontend/src/shared/config/api.ts) 直接消费生成的 v2 DTO，提供状态/模块读取、整体验证/应用、operation 回读、外部差异、文件还原和持久化重试入口；单模块入口把 module 同时绑定在 URL 与 typed payload。写请求仍由调用方决定发起次数，client 不做 mutation retry。

[`operation.ts`](../../../frontend/src/shared/config/operation.ts) 要求首次发送前固定 `operation_id`。apply 返回进行中时有界轮询；网络失败或 timeout 后只按同一 ID 查询，不重放 apply；`unknown` 另行回读活动状态，不能推断操作未执行。结果 ID 不一致按非法响应拒绝。表单 phase 保留打开时的双 revision 草稿，settled 前不把结果未知伪装成失败或成功。

[`query-keys.ts`](../../../frontend/src/shared/config/query-keys.ts) 将模块、活动 revision 和文件 revision 纳入 key；保存后只失效目标模块、确定的引用依赖、状态和概览，不清空全部查询。[`form-values.ts`](../../../frontend/src/shared/config/form-values.ts) 用 BigInt 做字节/duration 精确转换，区分继承与显式值、保留已删除引用占位，并在 variant 提交时只选择白名单字段；IP/CIDR helper 只负责词法检查，冲突和规范化仍以服务端为准。

[`ConfigFormModal`](../../../frontend/src/shared/components/ConfigFormModal.tsx) 统一受限高度、内部滚动、保存防重、脏关闭确认、安全错误与 request ID 展示，并提供字段路径到 Ant Design Form 的定位转换。它是业务表单容器而非 schema 自动表单；P3 各页面按自身领域上下界和类型分支使用该容器。

## P1 外部配置变化基础（2026-09-08）

[`external-change.ts`](../../../frontend/src/shared/config/external-change.ts) 从权威 `ConfigState` 派生文件变化、缺失、不可读、超限、已应用未同步和阻塞事实。关闭提示只记录当前事实 key，不清除 issue；活动/文件 revision 或文件状态变化后重新提示。差异响应必须与当前活动/观察 revision 同时匹配，否则进入冲突；还原返回成功后仍保留 issue 并等待下一份权威状态确认，不自行假定文件已同步。`FILE_REVISION_CONFLICT` 保留当前差异与脏草稿。

[`ExternalChangeBanner`](../../../frontend/src/shared/components/ExternalChangeBanner.tsx) 和 [`ExternalChangeDrawer`](../../../frontend/src/shared/components/ExternalChangeDrawer.tsx) 提供轻量提示、字段级差异/受保护变化展示、脏关闭确认、文件还原、组合采用及同步重试入口。还原确认明确只覆盖所见文件版本，不回滚运行态。[`operation.ts`](../../../frontend/src/shared/config/operation.ts) 对还原和持久化重试复用单次 mutation + operation 回读，不把“重试文件同步”变成配置重新应用。

[`ConfigFileStatus`](../../../frontend/src/shared/components/ConfigFileStatus.tsx) 已挂入受保护的 `AppLayout`，以 `configKeys.state()` 每 30 秒仅在页面可见时轮询正式 `/api/v2/config/state`；刷新失败显示可重试的全局提示，不阻断当前页面。发现 issue 后才读取绑定双 revision 的差异，外改不会自动进入 Runtime；还原固定新 `operation_id` 并显式确认丢弃外改，`applied_unpersisted` 重试严格复用原 ID、只调用文件 retry route。mutation 结束后失效差异并回读权威状态，Banner 不因 HTTP 成功提前消失；被拒绝、补偿失败、结果未知和版本冲突继续展示事实。

P3 全局协调器已把选择结果作为一次 typed Candidate 交给正式 validate/apply；普通模块保存遇到外改也依赖后端 `discard_external_changes` 确认，不伪造通用 YAML 或自动合并。App/MSW 覆盖组合采用、取消、二次外改冲突、还原和原 ID 持久化重试，纯函数覆盖十模块差异映射。

当前 production embed WebUI 连接 `_fluxdns/p1-config-runtime-live/` 的 loopback 后端完成真实登录；外改 logs 后，30 秒全局轮询显示 Banner，Drawer 读取 `logs` 差异和双 revision，确认还原后 Banner/Drawer 依据权威状态消失。源/派生文件恢复为 `debug` 与 `./logs/hot.log`，浏览器 warning/error 为 0，Runtime revision 前后保持 1，随后 UDP `localhost A` 仍返回 `127.0.0.1`，SQLite 布局仍为 `statistics-v2|1`。该浏览器证据未覆盖窄屏、刷新失败、二次冲突、持久化 retry 异常态或 P3 组合采用；这些状态保留自动测试或后续阶段验收。

## P1 系统运行状态（2026-09-08）

[`SystemPage`](../../../frontend/src/modules/system/SystemPage.tsx) 已从旧 `/system` 路由退出后的未挂载源码转为 `/system-runtime` 正式页面。进程数据由 [`getProcessMetrics`](../../../frontend/src/modules/system/api.ts) 读取 BC-23 的 `/api/v2/system/runtime`，沿共享 Bearer client 展示运行时长、RSS、CPU、线程与采样时间；现有 `/api/v1/system` 只补版本、启动时间和管理能力，两路失败可独立降级。

[`useProcessMetrics`](../../../frontend/src/modules/system/hooks.ts) 复用 30 秒可见性轮询和手动刷新；uptime 只从有效响应基准按接收时刻本地递增，隐藏页不逐秒渲染。RSS 格式化保留十进制 u64 字符串到 BigInt 的精度并统一显示 MiB；后端 measurement 的 `warmup`、`observation_gap`、`sampling_failed`、`unsupported` 原因显式呈现，不映射为零。页面没有 QPS/RPM、停止、重启或日志写操作。

Windows 真实浏览器使用当前 Vite 页面连接 `_fluxdns/fc14-ui-live-setup/` 的 loopback 后端，完成登录、导航、可用进程样本和手动刷新；实际 RSS/CPU/thread 及时间字段正确展示，刷新后 sample/uptime 推进，浏览器日志为空。该证据不覆盖窄屏或真实 OS 采样失败，后者仅由前端 fixture 与 BC-23 后端测试分别覆盖。

## P2 固定契约与只读基础（2026-09-08）

[`mocks/fixtures.ts`](../../../frontend/src/mocks/fixtures.ts) 新增严格绑定生成 v2 DTO 的服务指标、跨日记录、配置模块、系统白名单和保留状态样本。指标样本包含峰值、warmup 与观测缺口；记录样本包含同毫秒稳定 ID、原始身份与历史匹配分离、缓存生产者和截断 Answer。MSW 对应路由只返回固定契约数据，使用 Bearer 并保持 v2 `field_errors` 错误 envelope，不模拟分页、过滤或运行时 owner 已交付。

[`modules/dns-settings/api.ts`](../../../frontend/src/modules/dns-settings/api.ts) 暴露 DNS、统计、保留状态和正式 preview，[`modules/system-settings/api.ts`](../../../frontend/src/modules/system-settings/api.ts) 暴露系统白名单与 logs 模块读取；两者直接复用 `apiV2Request` 和配置模块 API。P3 页面已替换对应空态并接入编辑；FE-03/04 的 WS 实时能力仍不因此完成。

## P3 单模块写入与代理配置（2026-09-08）

[`shared/config/hooks.ts`](../../../frontend/src/shared/config/hooks.ts) 统一消费模块 `ConfigRead`、双 revision 和正式单模块 validate/apply。每次保存先预校验，再按后端返回的改名、listener 重绑、保留缩短或外部变化覆盖影响确认；`operation_id` 首次发送前固定，网络结果不明时只回读，不自动重放。成功后按类型化依赖失效 query；`applied_unpersisted` 保持独立警告并交给全局文件同步入口。

[`ProxiesPage`](../../../frontend/src/modules/proxies/ProxiesPage.tsx) 已替换 `/proxies` 空态，提供搜索、新建和按旧 name 编辑。表单只在 env/file 两类 SecretRef 来源间切换并提交当前分支，列表只显示引用位置和类型化引用数；实际 Secret 值不进入浏览器。MSW 交互测试检查单模块路径、预校验先于 apply、旧 name 和 SecretRef payload；真实后端热应用与文件证据见 P3 联合验收。

[`HostsPage`](../../../frontend/src/modules/hosts/HostsPage.tsx) 已替换 `/hosts` 空态，读取类型化 `const/file` 来源、引用数和 Runtime ready/stale/failed 状态。表单按来源只提交内联正文或文件路径/更新周期，并保留 `json/hosts` 格式；来源切换不会携带隐藏分支字段。

[`RuleSetsPage`](../../../frontend/src/modules/rule-sets/RuleSetsPage.tsx) 已替换 `/rule-sets` 空态，区分 `const/file/remote` 与 `json/clash/dat`，并显示远程代理、刷新计划及 Runtime stale/failed 状态。表单只提交当前来源字段；`clash` 保持行格式，`dat` 不作为 YAML/JSON 文本解析，也未增加主动刷新端点。

[`UpstreamsPage`](../../../frontend/src/modules/upstreams/UpstreamsPage.tsx) 已替换 `/upstreams` tab 空态，在同一模块读写 Hosts、DoH 和 Group。DoH 只提交当前 address/bootstrap/connect_ip/proxy/ECS 字段；组成员与 fallback 使用有序结构化名称/权重控件，类型和模式切换不携带隐藏字段。嵌套组、循环、模式权重和改名引用仍由后端完整候选权威校验。v2 OpenAPI 同批补充 Listener/Upstream discriminator mapping，生成类型现在使用线上真实 `udp/tcp/doh` 与 `hosts/doh/group`，不再误用 schema 名称作为 type 值。

[`StrategiesPage`](../../../frontend/src/modules/strategies/StrategiesPage.tsx) 已替换 `/strategies` 空态。规则表单保持顺序并区分 Hosts 本地回答和 rule_set+upstream，两类字段互斥；上移、下移和移除均更新整体候选。cache、TTL、ECS 明确区分继承、启用和禁用，不用空值代替继承。

[`ListenersPage`](../../../frontend/src/modules/listeners/ListenersPage.tsx) 已替换 `/listeners` 空态。UDP/TCP 编辑地址、端口、策略和可选 Hosts；DoH 编辑有序 route 及多个 endpoint，并按 TLS terminate/external、peer/forwarded_header/proxy_protocol 选择白名单字段。列表从 Runtime 投影显示实际 binding/accepting，保存后的物理冲突、差量重绑和补偿仍由后端 prepare/owner 决定。

[`ClientsPage`](../../../frontend/src/modules/clients/ClientsPage.tsx) 已替换 `/clients` 空态，列表同时展示唯一管理 name、请求匹配 `client_id` 和 IP/CIDR。创建时输入 ID，编辑时 ID 控件只读且 payload 通过 `clientEditValue` 剔除；name、IP、策略及 cache/TTL/ECS 覆盖按旧 name 提交，不重写历史身份。

[`DnsSettingsPage`](../../../frontend/src/modules/dns-settings/DnsSettingsPage.tsx) 已替换 `/dns-settings` 空态，分区编辑缓存/快照、TTL、ECS、详情记录和 R/G/T。保留保存前调用正式 preview 获取真实 SQLite/WAL 字节与候选 UTC cutoff，并把结果并入后端 `retention_shortening` 确认；浏览器不自行计算权威水位，保存也不触发立即清理。

[`SystemSettingsPage`](../../../frontend/src/modules/system-settings/SystemSettingsPage.tsx) 已替换 `/system-settings` 空态。`work/rules/database/records` 活动源路径表达与 WebUI 监听来自 `SystemConfigRead` 且保持只读，不冒充 Runtime 解析后的绝对路径；日志 `enable/level/path` 单独通过 `logs` 模块候选预校验、热应用、持久化和回显，不向启动配置字段提供伪编辑入口。

FC-16 组合采用把外部差异中的同名资源转换为带明确 `original_name` 的 update，仅外部资源转换为 create；仅活动资源保持禁选，不推断删除。客户端 update 剔除只读 `client_id`。用户可跨模块勾选白名单变化，一次提交全局 Candidate；未选差异与 `work/database/webui/protected_credentials` 受保护变化通过 `discard_external_changes` 确认后按活动配置还原，二次外改继续由 file revision 冲突阻断。

## P3 联合验收（2026-09-08）

Windows 内嵌 WebUI 连接 `_fluxdns/p3-live/` 的真实 ConfigV2 进程。脚本按依赖对十模块逐一执行 Bearer module GET、validate、202 apply、operation poll 与 GET 回显，全部得到 `applied_synced`；真实 UDP/SQLite/保留/历史结果及外部文件组合采用、二次冲突和 restore 见[后端 Management 实现](../backend/management.md#p1-配置事务与文件操作2026-09-08)。

浏览器在 1440×900 逐项进入 12 个一级路由，均出现正式标题且无 `PendingModulePage` 或页面级横向溢出。日志表单真实保存发出 Bearer module GET、validate 200、apply 202、operation 200 和回显 200；DNS 保留表单确认 preview 200 严格先于 statistics validate/apply。真实外改后的 Drawer 显示 Hosts 与 logs 字段级差异，2 项全局组合采用经确认成功并自动关闭。390×844 下 DNS/Hosts 无页面级横向溢出，导航使用移动 Drawer，表格仅在自身容器滚动，编辑弹窗完整可操作；Console error/warning 为空。

本次不实现或验证 WS/P4、P5、BC-27、HTTPS 反向代理、Linux/macOS、磁盘满和约 10 客户端/core 2ms 性能。浏览器使用 loopback HTTP 与测试账号，未把 access token 写入日志、文档或浏览器持久存储。

## 能力与证据

2026-09-07 P0 补充：[`generated-v2.ts`](../../../frontend/src/shared/api/generated-v2.ts) 由 [v2 OpenAPI](../../../frontend/openapi/management-api-v2.yaml) 生成，只有新契约模块消费。现有 `apiRequest`、AuthProvider、Vite 代理、mock 和 App 路由未切换；新增 `apiV2Request` 仅由明确的新版模块调用，不提供运行时 v1/v2 选择开关。

[`route-contract.ts`](../../../frontend/src/app/route-contract.ts) 固定 12 个一级路径与配置模块映射，保留 `/dashboard`、`/queries`；上游组仅为 `/upstreams` 页内 tab。App 与导航消费该表，P3 九个配置入口均已挂载领域页面；路径存在仍不能替代其真实读写与浏览器证据。

[`shared/config/contract.ts`](../../../frontend/src/shared/config/contract.ts) 直接消费生成类型：草稿固定双 revision，区分预校验/确认/应用/结果未知；客户端普通编辑白名单剔除 `client_id`；操作结果区分同步、仅重试持久化、回读活动值和阻塞；大整数转表单前检查安全范围。FC-02 已补配置 client、操作回读、query key/精确失效、共享值转换和 Modal 容器；全局文件状态由壳层协调器消费 TanStack Query，不另建可变配置权威。P3 各领域表单复用该链路。

FC-02 定向 Vitest 共 27 项，覆盖 v2 Bearer 路径、字段错误、配置 endpoint、operation 单次发送/回读/unknown、query key/失效、单位/duration/IP/继承/variant 及脏关闭确认；与 Rust 共用的 schema 夹具测试见[交付实现](../delivery.md#前端与接口生成)。这些验证使用 MSW/jsdom，不证明后端配置 route、真实文件、浏览器路由离开或内嵌环境已经接线。

| 能力 | 代码实现 | 正式入口接线 | 验证证据 | 已知限制 |
| --- | --- | --- | --- | --- |
| setup/session gate | AuthProvider + ProtectedRoute | bootstrap 的 provider/router | P1 认证测试及真实初始化/登录/刷新/登出；P3 内嵌深链接重载恢复 | 外部 HTTPS 代理未验收 |
| 同源请求/取消 | `apiRequest`、unauthorized listener | 各 module API 共用 client | P1 并发刷新/取消/迟到结果测试及真实 Bearer 请求头观察 | 普通泛型响应不是完整运行时 schema 校验 |
| 退出数据清理 | `performLogout` finally | AppLayout 使用 auth logout | 本轮核对实际分支 | 401 与 logout 清理行为不同，不能混写 |
| P3 v2 配置页面 | 九个领域页面、generated-v2、module hooks | 受保护路由与十模块正式 API | MSW/完整 Vitest、真实 Bearer/文件/SQLite/UDP/浏览器，见 P3 联合验收 | 不包含 WS/P4 或 P5 收口 |
| mock 隔离 | bootstrap DEV gate、Vite 构建 | 显式开发变量启用 | 本轮静态 | mock 不证明后端集成或安全验收 |
| 12 路由壳层 | route-contract、App、AppLayout、九个配置页 | 受保护路由与分组导航 | 1440×900 逐路由标题/溢出检查和 390×844 移动导航 | Dashboard/queries 的 WS 实时能力未进入 P3 |
| 配置交互基础 | config api/operation/query keys/form values、ConfigFormModal | 全局 state/operation 与十模块领域表单 | 91 项完整 Vitest、typecheck/build、真实 Bearer 单模块读写回显 | 运行时响应仍由后端 schema/owner 权威校验 |
| 外部变化处理 | ConfigFileStatus、external adoption、Banner/Drawer | 轮询、差异、还原/retry、覆盖确认和组合采用 | MSW 二次冲突；真实双模块外改/采用/冲突/restore 与浏览器 Drawer | WS 文件通知未授权，仍以 HTTP 轮询 |
| 系统运行状态 | system Page/hooks/api、共享 formatters | `/system-runtime` 读取 v2 进程指标和 v1 基础信息 | FC-14 测试及 Windows 真实浏览器/后端可用样本；P3 窄屏无溢出 | 真实不可用 OS 样本和 Linux 未做浏览器验收 |

2026-09-05 原核对未运行 pnpm 或浏览器；P1/P2 历史证据及 P3 联合验收分别见上节。环境与打包边界见[交付证据](../delivery.md)，P3 真实证据不外推为 P4/P5 或跨平台完成。
