# 前端设计

> 文档状态：有效
>
> 适用范围：WebUI 分层、状态所有权、路由、接口与展示约束
>
> 最后评审：2026-09-05（既有前端边界拆分复核）

## 设计结论

前端是 React + TypeScript + Vite 的独立 SPA，使用 React Router、TanStack Query 与 Ant Design。它面向反复查看运行状态的管理场景，不承担 DNS 协议、配置继承或上游选择逻辑。确切依赖版本以 [package.json](../../frontend/package.json) 和 [锁文件](../../frontend/pnpm-lock.yaml) 为准。

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
- `shared/api` 集中同源路径、Cookie、取消、超时与错误转换；不内置任意生产 baseURL。
- OpenAPI 是接口字段唯一权威，生成的 TypeScript 不手工改；fixture 遵守同一契约但不能作为服务已接线的证据。
- 后端状态保持 `available/unavailable`、健康、stale、gap 等语义，不能把不可用数据显示为正常零值。

## 认证与路由约束

先查询 setup 状态，再决定初始化、会话恢复与受保护页面。setup 未决时不请求受保护数据；初始化成功发布 setup ready 和 session；竞争冲突重新读取状态，不无限重试写入。

未认证用户进入登录；请求 `401` 由统一认证边界回收会话、取消查询并交给 guard 跳转。退出需要清理前一个用户的查询数据。loading、error、setup-required、unauthenticated 和正常内容必须有明确状态，不能把失败当成未登录或空数据。

Cookie、密码、Origin 与会话安全唯一维护于 [Management 设计](management.md)。实际 AuthProvider 与路由行为见[应用实现](../implementation/frontend/application.md)。

## 查询与呈现

- 摘要可以在页面可见时轮询，后台窗口停止定时请求；详情/筛选页面以显式参数和用户刷新为主。
- 查询 key 包含分页、排序、过滤和时间范围，不能让旧请求覆盖新条件；取消、认证失败和不可重试错误不机械重试。
- 错误保留安全 request ID 与 retry 语义，loading/error/empty/unavailable 分开呈现；时间和 duration 由统一 formatter 转换。
- 不渲染后端返回的 HTML；qname、answer 等请求内容作为文本显示。历史空详情明确标识，不构造虚假的域名或响应。
- 页面应支持窄屏、表格横向查看、键盘访问与明确状态，不用营销式大块说明替代管理操作。

页面范围是初始化/登录和七个只读管理页。没有配置编辑、用户管理、资源强制刷新、缓存清除或 DNS query 工具；新增写入需要先评审权限、审计、CSRF、revision/conflict、幂等和回滚契约。

## 交付与验证边界

开发代理和 mock 只是工程模式，生产浏览器始终访问同源 `/api/v1`。SPA 通过 `webui-embed` 可内嵌发布，API 与静态 fallback 独立分流；操作步骤见[交付实现](../implementation/delivery.md)。

组件测试、schema 类型生成和 mock 不能替代真实浏览器的 Cookie、Network/Storage、初始化跳转和安全观察。验证边界见[交付实现](../implementation/delivery.md)，页面与查询接线见[页面实现](../implementation/frontend/pages.md)。
