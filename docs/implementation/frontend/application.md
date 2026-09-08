# 前端应用与认证实现

> 文档状态：有效
>
> 适用范围：前端 bootstrap、provider、路由鉴权、HTTP client 与会话回收
>
> 最后核对：2026-09-08（P1 壳层、认证、配置基础与系统运行状态接线核对）
>
> 核对基线：`a47725dbcf57e000213ba1dacb72ef189fc14d7a`

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

接口类型来自 [OpenAPI](../../../frontend/openapi/management-api-v1.yaml) 生成的 [`generated.ts`](../../../frontend/src/shared/api/generated.ts)，[`types.ts`](../../../frontend/src/shared/api/types.ts) 提供前端投影。schema 改动后使用 `generate:api`，命令见[前端 README](../../../frontend/README.md)。

## P1 Bearer 接线（2026-09-08）

[`auth/api.ts`](../../../frontend/src/modules/auth/api.ts) 消费初始化/登录的 `AuthSession`，access token 仅存于共享 client 的模块内存；返回 AuthProvider/查询缓存前重新投影 `user/expires_at`，不透传 token 或额外 session 字段。业务请求和 `GET auth/session` 只附加 Authorization Bearer、明确省略 Cookie。页面重载后，client 先调用同源 `POST auth/refresh` 恢复访问凭据，不读取 HttpOnly Cookie 或浏览器持久存储。

同一认证代次内所有请求共享一次在途刷新，刷新最多 5 秒且各等待方仍受自己的 10 秒/调用者取消约束。一个请求取消不终止其他等待者；登出/401 增加认证代次并禁止迟到刷新恢复会话，新登录不受旧请求迟到 401 影响。刷新只发生在业务请求发送前；已发出的请求返回 401/500 或结果未知均不自动重放。登出仍清空本地状态，失败不等于服务端已撤销，沿用上节的错误边界。

mock 的业务 handler 也要求 Bearer，但其 Cookie/Origin 只由测试状态模拟，不充当生产替代。Vite 继续把 `/api` 透明代理到既有后端，未硬编码令牌或生产 baseURL；现有页面与认证 handler 保持 v1，配置公共 client 明确选择 v2，不提供运行时任意版本开关。Bearer 测试覆盖并发、取消、迟到结果、写请求不重放、无 token session 投影和登录/登出流程。

真实内嵌 WebUI 的浏览器验证覆盖初始化、页面重载后的 Cookie 刷新/Bearer 业务请求、登出后刷新保持未登录、再次登录及 Cookie 清除。开发者接口只读确认 localStorage/sessionStorage 条目均为 0，`document.cookie` 不可读刷新凭据；Network 只记录请求头是否存在，不输出 token。该验证使用旧壳层的真实后端数据，不证明 FC-01 十二路由、FC-02 公共表单或 v2 配置接口完成。

## P1 应用壳层（2026-09-08）

[`AppLayout`](../../../frontend/src/shared/components/AppLayout.tsx) 从同一 `managementRoutes` 契约生成“监控 / DNS 管理 / 系统”三组 12 个一级入口，使用 Lucide 图标、浅色侧栏、面包屑、当前用户与图标化登出/折叠控件。桌面侧栏独立滚动；小于 720px 时改用 Drawer，不缩放固定宽画布。未知路径不选择任一菜单项，旧 `/runtime`、`/health`、`/statistics`、`/resources`、`/system` 路径不兼容跳转。

`/dashboard` 和 `/queries` 继续消费当前 v1 真实只读数据；FC-14 又将 `/system-runtime` 接入真实 v2 进程读数。其余九个入口不加载或伪造图稿业务数据，其中八个使用 [`PendingModulePage`](../../../frontend/src/app/PendingModulePage.tsx)，`/upstreams` 仅接入“上游 / 上游组”页内 tab 壳层，`tab=groups` 进入浏览器历史；业务列表和表单仍归 FC-06。主题 token 改为浅灰导航、白工作区、蓝色主操作及独立成功/警告/错误色；未增加暗色全站主题。

Windows 浏览器 fixture 验证覆盖默认桌面、390×844、移动 Drawer 跳转、上游 tab URL、Console warning/error 为空；Vitest 定向路由测试 22 项通过。fixture 不证明 v2 handler、真实配置页面、深链接静态 fallback 或生产内嵌资源已接线；已接入页面仍使用 `/api/v1`。

## P1 配置交互基础（2026-09-08）

[`shared/config/api.ts`](../../../frontend/src/shared/config/api.ts) 直接消费生成的 v2 DTO，提供状态/模块读取、整体验证/应用、operation 回读、外部差异、文件还原和持久化重试入口；单模块入口把 module 同时绑定在 URL 与 typed payload。写请求仍由调用方决定发起次数，client 不做 mutation retry。

[`operation.ts`](../../../frontend/src/shared/config/operation.ts) 要求首次发送前固定 `operation_id`。apply 返回进行中时有界轮询；网络失败或 timeout 后只按同一 ID 查询，不重放 apply；`unknown` 另行回读活动状态，不能推断操作未执行。结果 ID 不一致按非法响应拒绝。表单 phase 保留打开时的双 revision 草稿，settled 前不把结果未知伪装成失败或成功。

[`query-keys.ts`](../../../frontend/src/shared/config/query-keys.ts) 将模块、活动 revision 和文件 revision 纳入 key；保存后只失效目标模块、确定的引用依赖、状态和概览，不清空全部查询。[`form-values.ts`](../../../frontend/src/shared/config/form-values.ts) 用 BigInt 做字节/duration 精确转换，区分继承与显式值、保留已删除引用占位，并在 variant 提交时只选择白名单字段；IP/CIDR helper 只负责词法检查，冲突和规范化仍以服务端为准。

[`ConfigFormModal`](../../../frontend/src/shared/components/ConfigFormModal.tsx) 统一受限高度、内部滚动、保存防重、脏关闭确认、安全错误与 request ID 展示，并提供字段路径到 Ant Design Form 的定位转换。它是业务表单容器而非 schema 自动表单；当前尚未挂到未交付的配置页面，路由离开 guard 和领域上下界由后续各模块接入。

## P1 外部配置变化基础（2026-09-08）

[`external-change.ts`](../../../frontend/src/shared/config/external-change.ts) 从权威 `ConfigState` 派生文件变化、缺失、不可读、超限、已应用未同步和阻塞事实。关闭提示只记录当前事实 key，不清除 issue；活动/文件 revision 或文件状态变化后重新提示。差异响应必须与当前活动/观察 revision 同时匹配，否则进入冲突；还原返回成功后仍保留 issue 并等待下一份权威状态确认，不自行假定文件已同步。`FILE_REVISION_CONFLICT` 保留当前差异与脏草稿。

[`ExternalChangeBanner`](../../../frontend/src/shared/components/ExternalChangeBanner.tsx) 和 [`ExternalChangeDrawer`](../../../frontend/src/shared/components/ExternalChangeDrawer.tsx) 提供轻量提示、差异/受保护变化展示、脏关闭确认、文件还原确认及可选的组合采用/同步重试入口。还原确认明确只覆盖所见文件版本，不回滚运行态。[`operation.ts`](../../../frontend/src/shared/config/operation.ts) 对还原和持久化重试复用单次 mutation + operation 回读，不把“重试文件同步”变成配置重新应用。

这组基础当前没有挂入 `AppLayout`，也没有启动全局 polling：后端 BC-30 尚未注册正式配置状态/差异/文件操作 route，提前挂载只会制造持续失败请求。各模块的组合差异编辑仍需 FC-05 至 FC-13 提供真实领域表单；当前组件只暴露可选入口，不伪造通用 YAML 或自动合并能力。

## P1 系统运行状态（2026-09-08）

[`SystemPage`](../../../frontend/src/modules/system/SystemPage.tsx) 已从旧 `/system` 路由退出后的未挂载源码转为 `/system-runtime` 正式页面。进程数据由 [`getProcessMetrics`](../../../frontend/src/modules/system/api.ts) 读取 BC-23 的 `/api/v2/system/runtime`，沿共享 Bearer client 展示运行时长、RSS、CPU、线程与采样时间；现有 `/api/v1/system` 只补版本、启动时间和管理能力，两路失败可独立降级。

[`useProcessMetrics`](../../../frontend/src/modules/system/hooks.ts) 复用 30 秒可见性轮询和手动刷新；uptime 只从有效响应基准按接收时刻本地递增，隐藏页不逐秒渲染。RSS 格式化保留十进制 u64 字符串到 BigInt 的精度并统一显示 MiB；后端 measurement 的 `warmup`、`observation_gap`、`sampling_failed`、`unsupported` 原因显式呈现，不映射为零。页面没有 QPS/RPM、停止、重启或日志写操作。

Windows 真实浏览器使用当前 Vite 页面连接 `_fluxdns/fc14-ui-live-setup/` 的 loopback 后端，完成登录、导航、可用进程样本和手动刷新；实际 RSS/CPU/thread 及时间字段正确展示，刷新后 sample/uptime 推进，浏览器日志为空。该证据不覆盖窄屏或真实 OS 采样失败，后者仅由前端 fixture 与 BC-23 后端测试分别覆盖。

## 能力与证据

2026-09-07 P0 补充：[`generated-v2.ts`](../../../frontend/src/shared/api/generated-v2.ts) 由 [v2 OpenAPI](../../../frontend/openapi/management-api-v2.yaml) 生成，只有新契约模块消费。现有 `apiRequest`、AuthProvider、Vite 代理、mock 和 App 路由未切换；新增 `apiV2Request` 仅由明确的新版模块调用，不提供运行时 v1/v2 选择开关。

[`route-contract.ts`](../../../frontend/src/app/route-contract.ts) 固定 12 个一级路径与配置模块映射，保留 `/dashboard`、`/queries`；上游组仅为 `/upstreams` 页内 tab。App 与导航已消费该表，但未就绪入口只是明确空态，不能将路径存在计为业务页面完成。

[`shared/config/contract.ts`](../../../frontend/src/shared/config/contract.ts) 直接消费生成类型：草稿固定双 revision，区分预校验/确认/应用/结果未知；客户端普通编辑白名单剔除 `client_id`；操作结果区分同步、仅重试持久化、回读活动值和阻塞；大整数转表单前检查安全范围。FC-02 已补配置 client、操作回读、query key/精确失效、共享值转换和 Modal 容器，但没有可变全局配置 store；外部差异工作区与生产 owner 接线仍是独立边界。

FC-02 定向 Vitest 共 27 项，覆盖 v2 Bearer 路径、字段错误、配置 endpoint、operation 单次发送/回读/unknown、query key/失效、单位/duration/IP/继承/variant 及脏关闭确认；与 Rust 共用的 schema 夹具测试见[交付实现](../delivery.md#前端与接口生成)。这些验证使用 MSW/jsdom，不证明后端配置 route、真实文件、浏览器路由离开或内嵌环境已经接线。

| 能力 | 代码实现 | 正式入口接线 | 验证证据 | 已知限制 |
| --- | --- | --- | --- | --- |
| setup/session gate | AuthProvider + ProtectedRoute | bootstrap 的 provider/router | P1 认证测试及真实初始化/登录/刷新/登出，见上节 | v2 切换与生产深链接未验收 |
| 同源请求/取消 | `apiRequest`、unauthorized listener | 各 module API 共用 client | P1 并发刷新/取消/迟到结果测试及真实 Bearer 请求头观察 | 普通泛型响应不是完整运行时 schema 校验 |
| 退出数据清理 | `performLogout` finally | AppLayout 使用 auth logout | 本轮核对实际分支 | 401 与 logout 清理行为不同，不能混写 |
| mock 隔离 | bootstrap DEV gate、Vite 构建 | 显式开发变量启用 | 本轮静态 | mock 不证明后端集成或安全验收 |
| 12 路由壳层 | route-contract、App、AppLayout、PendingModulePage | 受保护路由与分组导航 | 23 项路由测试；桌面/390×844 fixture 浏览器与 Console 检查沿用 FC-01 证据 | 八个入口为空态，upstreams 仍只有 tab 壳层 |
| 配置交互基础 | config api/operation/query keys/form values、ConfigFormModal | 仅供后续模块调用，未挂载业务页 | FC-02 定向 Vitest 27 项、typecheck | MSW/jsdom；正式 v2 配置 route、真实文件和浏览器交互未验收 |
| 外部变化基础 | external-change reducer、Banner/Drawer、文件单次 mutation 回读 | 未挂 AppLayout，等待 BC-30 | FC-16 基础定向 Vitest 8 项、typecheck | 无真实文件/后端/browser；组合采用等待领域表单 |
| 系统运行状态 | system Page/hooks/api、共享 formatters | `/system-runtime` 读取 v2 进程指标和 v1 基础信息 | FC-14 定向 27 项；完整 Vitest 19 文件 80 项；typecheck/build；Windows 真实浏览器/后端可用样本 | 窄屏和真实不可用 OS 样本未做浏览器验收；其他 v2 页面未接线 |

2026-09-05 原核对未运行 pnpm 或浏览器；P1 新增的 Bearer、壳层、FC-02/16 和 FC-14 证据见上节。历史记录与尚无运行证据的环境边界见[交付证据](../delivery.md)，不把共享组件/MSW 回归算作整套 v2 生产切换或其他业务页面完成。
