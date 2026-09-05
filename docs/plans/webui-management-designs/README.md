# WebUI 管理后台视觉草案

> 文档状态：草案
>
> 适用范围：WebUI 管理后台重构计划的视觉评审图稿、用途与生命周期

本目录集中存放 [WebUI 管理后台重构需求](../webui-management-requirements.md)的评审图稿。图稿用于确认视觉方向，不代表已接受的详细设计、当前实现或运行验收结果。

共 28 张桌面 SVG，覆盖全部 12 个一级模块。打开[本地评审总览](review.html)可按模块浏览、点击缩略图查看原图；不依赖网络、脚本或开发服务器。SVG 为可编辑源文件，本地 PNG 预览与检查产物不纳入版本控制。

## 本轮补齐

新增 9 个一级页面、1 个上游组标签页及 12 张配套编辑视图。详细语义、现有配置限制与尚未展开的状态以[需求正文](../webui-management-requirements.md#77-监听入口)为准。

| 模块 | 一级页面 | 配套编辑视图 |
| --- | --- | --- |
| 监听入口 | [入口列表](webui-listeners.svg)：协议、绑定地址、策略及 ECS 继承 | [UDP/TCP 编辑](webui-listener-editor.svg)、[DoH 编辑](webui-doh-listener-editor.svg) |
| DNS 配置 | [全局概览](webui-dns-settings.svg)：缓存、TTL/ECS 与解析记录 | [缓存](webui-dns-cache-editor.svg)、[TTL/ECS](webui-dns-policy-editor.svg)、[解析记录配置](webui-dns-recording-editor.svg) |
| DNS 分流策略 | [策略列表](webui-dns-strategies.svg)：规则数、默认上游及覆盖关系 | [策略编辑](webui-strategy-editor.svg)：有序匹配表与独立覆盖项 |
| Hosts 配置 | [资源列表](webui-hosts.svg)：内联/文件来源及重载状态 | [内联编辑](webui-hosts-editor.svg)：结构化主机映射 |
| 规则集 | [规则集列表](webui-rule-sets.svg)：来源、格式、刷新计划与陈旧快照 | [远程规则编辑](webui-rule-set-editor.svg)：URL、代理与定时更新 |
| 客户端配置 | [客户端列表](webui-clients.svg)：标识/CIDR、策略及缓存/ECS | [客户端编辑](webui-client-editor.svg)：匹配条件与继承/覆盖 |
| 代理配置 | [代理列表](webui-proxies.svg)：协议族、SecretRef 来源和引用 | [代理编辑](webui-proxy-editor.svg)：仅编辑环境变量或文件引用 |
| 系统配置 | [系统概览](webui-system-settings.svg)：日志可编辑，其余分区只读 | [日志编辑](webui-logs-editor.svg)：开关、级别、路径及重启提示 |
| 系统运行状态 | [进程概览](webui-system-runtime.svg)：时长、内存、线程与基础信息 | 无配置操作 |
| DNS 上游 | [上游组标签页](webui-upstream-groups.svg)：主成员、选择模式和回退 | [上游组编辑](webui-upstream-group-editor.svg)：保留旧名称及模式约束 |

## 既有图稿

| 图稿 | 用途 |
| --- | --- |
| [服务状态](webui-service-status.svg) | 浅色模式：四项关键指标、轻面积填充的 QPS 主线与 RPM 辅助曲线、选中时刻读数；不含 WebSocket 状态或监听入口摘要 |
| [服务状态 · 深色模式](webui-service-status-dark.svg) | 相同布局与数据，评审中性深色背景、文字对比度与曲线配色 |
| [DNS 上游](webui-dns-upstreams.svg) | 展示上游和上游组的浏览、筛选、配置列表与编辑入口 |
| [DNS 上游编辑](webui-doh-upstream-editor.svg) | 以 DoH 为当前类型，展示名称修改、类型选择及连接、代理出口和 ECS 配置；改名保存必须携带旧名称，约束见需求正文 |
| [解析记录列表](webui-query-records.svg) | 倒序列表、查询筛选、结果摘要、来源和耗时，覆盖正常、异常、截断与历史未保留状态 |
| [解析记录悬浮详情](webui-query-record-details.svg) | 结果单元格锚定的非模态浮层，展示请求上下文及 Answer 明细；查看期间保持记录稳定 |

## 评审边界

全部画布为 1600 × 1040，统一字体、导航、状态色、分区、列表与弹窗风格。浅色图是本轮基准；深色风格仅提供既有服务状态样例，不暗示其余页面已完成深色适配。

图中使用演示地址、路径、变量名和数值。配置来源切换、展开字段、空态、错误态、窄屏和真实交互仍需后续详细设计；图中按钮不执行配置操作。本轮验证范围为 SVG 结构、栅格化视觉检查与文档链接，不包含前后端实现或运行验收。

本目录与对应活动计划保持相同生命周期。计划完成后删除仅用于评审的图稿；仍有长期设计价值的图稿迁入对应 `docs/architecture/` 文档范围并同步引用。
