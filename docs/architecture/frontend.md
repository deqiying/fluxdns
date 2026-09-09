# 前端设计

> 文档状态：有效
>
> 适用范围：WebUI 分层、状态所有权、路由、接口与展示约束
>
> 最后评审：2026-09-09（P4 共享实时连接、记录缓冲与稳定详情）

## 设计结论

前端是 React + TypeScript + Vite 的独立 SPA，使用 React Router、TanStack Query、Ant Design 与 Lucide 图标。它面向反复查看运行状态的管理场景，不承担 DNS 协议、配置继承或上游选择逻辑。确切依赖版本以 [package.json](../../frontend/package.json) 和 [锁文件](../../frontend/pnpm-lock.yaml) 为准。

不为已有查询数据额外建立全局 store。React Context/Hooks 保存会话和局部交互，TanStack Query 管理服务端快照；只有明确的新客户端状态需求才评估新增状态库。

## 分层与所有权

```text
app: providers / router / error boundary
 -> modules: page / hook / API projection
 -> shared: HTTP client / generated types / components / formatters
 -> same-origin Management API
```

- `app` 只组装 provider、路由、错误边界和应用生命周期，不包含页面业务。
- `modules` 按页面领域组织，查询键覆盖过滤/分页参数；页面不直接散落 fetch。
- `shared/api` 集中同源路径、内存 Bearer、认证刷新、取消、超时与错误转换；业务请求不携带 Cookie，不内置任意生产 baseURL。
- `shared/api` 还持有唯一按需 WS client：页面只注册订阅，不各自维护 socket、ticket、认证代次或重连循环；最后一个订阅退出后关闭连接。
- OpenAPI 是接口字段唯一权威，生成的 TypeScript 不手工改；fixture 遵守同一契约但不能作为服务已接线的证据。
- 后端状态保持 `available/unavailable`、健康、stale、gap 等语义，不能把不可用数据显示为正常零值。

## 认证与路由约束

先查询 setup 状态，再决定初始化、会话恢复与受保护页面。setup 未决时不请求受保护数据；初始化成功发布 setup ready 和 session；竞争冲突重新读取状态，不无限重试写入。

未认证用户进入登录；请求 `401` 由统一认证边界回收会话、取消查询并交给 guard 跳转。退出需要清理前一个用户的查询数据。loading、error、setup-required、unauthenticated 和正常内容必须有明确状态，不能把失败当成未登录或空数据。

Bearer、刷新 Cookie、密码、Origin 与会话安全唯一维护于 [Management 设计](management.md)。AuthProvider 只持有无 token 的 session 投影；客户端共享刷新有独立有界 deadline，各等待者取消互不影响，业务写请求不会自动重放。实际行为见[应用实现](../implementation/frontend/application.md)。

浏览器 WS 不持久化 access token，也不把 token 放入 URL。共享 client 使用现有内存 Bearer 调用 ticket 端点，并以固定协议名和短期单次 ticket 两个 subprotocol 创建连接；认证代次变化立即丢弃 socket、重连计时器和旧消息。401/4401 统一进入现有会话失效边界，不能在 WS 层建立第二套登录状态。

## 查询与呈现

- 摘要可以在页面可见时轮询，后台窗口停止定时请求；详情/筛选页面以显式参数和用户刷新为主。
- 查询 key 包含分页、排序、过滤和时间范围，不能让旧请求覆盖新条件；取消、认证失败和不可重试错误不机械重试。
- 服务状态先读 HTTP 权威快照再订阅实时指标；页面隐藏时释放订阅，恢复可见时重新取快照后再接续，不能用零填补断流区间。
- 解析记录默认关闭实时。开启后以 HTTP snapshot cursor 和 retention revision 订阅；500 条或 2 MiB 客户端缓冲先到者触发 resync。浮层或历史页打开时新记录只进入缓冲，固定记录 ID、目录快照和当前列表，显式操作后才回到最新首屏。
- 错误保留安全 request ID 与 retry 语义，loading/error/empty/unavailable 分开呈现；时间和 duration 由统一 formatter 转换。
- 不渲染后端返回的 HTML；qname、answer 等请求内容作为文本显示。历史空详情明确标识，不构造虚假的域名或响应。
- 页面应支持窄屏、表格横向查看、键盘访问与明确状态，不用营销式大块说明替代管理操作。

受保护壳层按监控、DNS 管理、系统三组提供 12 个一级入口；上游组属于 DNS 上游页内 tab，不增加第 13 个入口。未接入真实数据源的页面必须明确不可用，不复制设计图演示内容。桌面侧栏和窄屏 Drawer 使用同一路由契约，具体接线事实见[应用实现](../implementation/frontend/application.md)。唯一目标字段权威仍为 [v2 OpenAPI](../../frontend/openapi/management-api-v2.yaml)。

目标表单固定打开时的活动/文件 revision，不能被 refetch 覆盖脏草稿；`name` 改名保留 original_name，客户端 ID 编辑只读。响应丢失进入结果未知并查询 operation，不能自动重放；运行成功但文件未同步独立展示并只重试同步。外部差异处理是现有壳层工作区，不新增一级模块；不增加角色、通用 YAML 编辑或顶层删除。

## 交付与验证边界

开发代理和 mock 只是工程模式。全部认证、服务状态、解析记录、系统信息和配置页面只访问同源 `/api/v2`，不保留旧页面或 API client。鉴权、client、代理、mock 与 SPA fallback 必须成套维护。SPA 通过 `webui-embed` 内嵌发布，API 与静态 fallback 独立分流；操作步骤见[交付实现](../implementation/delivery.md)。

组件测试、schema 类型生成和 mock 不能替代真实浏览器的 Cookie、Network/Storage、初始化跳转和安全观察。验证边界见[交付实现](../implementation/delivery.md)，页面与查询接线见[页面实现](../implementation/frontend/pages.md)。
